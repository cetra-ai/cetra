# Monitoring: health checks and Prometheus metrics

cetra has two health endpoints on the HTTP port (8080), and an optional Prometheus endpoint on its
own port. Both are described here. The design is spec 12 § Process lifecycle and § Self-monitoring.

## Health endpoints

| Endpoint | Answers 200 when | Use it for |
|---|---|---|
| `/healthz` | the process is alive and no storage failure that only a restart clears is set | liveness |
| `/readyz` | every readiness check passes (see below) | readiness |

Both answer 503 with a `busy` problem when they fail. The detail names the failed checks and never a
key, a path or an error text. `/healthz` never does I/O, so it stays fast.

**`/healthz` (liveness).** It fails only on the sticky storage failures: `spans WAL`, `scores WAL`,
`object fsync` and `catalog`. Each of them stays set until the process restarts, so the detail reads
`restart needed: <checks>`. A restart is the remedy: the WAL is replayed and the catalog reconnects.
Under Kubernetes a failing `/healthz` restarts the pod. Under Docker, the `cetra health` healthcheck
reads `/readyz`, not `/healthz`, so a failing liveness check does not restart a container by itself.

**`/readyz` (readiness).** The checks run in this order. Each failure adds its name to the detail:

| Check | Fails when |
|---|---|
| `meta.db` | the metadata database does not answer |
| `spans WAL`, `scores WAL` | a WAL write failed (sticky) |
| `object fsync` | a local object fsync failed (sticky) |
| `catalog` | a catalog commit failed with an I/O error (sticky) |
| `free disk` | free space is below `storage.min_free_bytes`, so ingestion is refused |
| `object store` | the object store did not answer a probe within 2 s, and the result is cached 10 s |

The object-store check reads a marker object, `_probe/readyz`, in the bucket: it sends a HEAD and, if
that fails, a PUT of an empty object. The marker is written on the first check. It needs no permission
beyond the server's own.

A readiness failure does not restart anything. What it does depends on where cetra runs:

- **Kubernetes.** A full disk or an unreachable bucket takes the pod out of the Service until it
  recovers. With the single replica, the UI is then unreachable through the Service. Free space with
  `kubectl exec` or a bigger volume, and alert on `cetra_storage_disk_free_bytes` before the disk
  fills (see the examples below).
- **Docker and compose.** The container shows `unhealthy` and keeps running.

A healthy server with restore mode on also shows `"restore_mode": true` in its `/readyz` answer (see
[backup.md](backup.md#restore-mode-restore-drills)).

## Prometheus metrics

Metrics are off by default. Turn them on with `metrics_addr` (TOML), `CETRA_METRICS_ADDR`, or the
Helm chart's `metrics.enabled`:

```bash
# The environment, for example in cetra.env or a container
CETRA_METRICS_ADDR=127.0.0.1:9464
```

- The endpoint is a separate listener, `GET /metrics`, in OpenMetrics text. Anything else answers 404.
  If the encoding fails, it answers 500 with an empty body.
- **It has no authentication.** Bind it to a private address (`127.0.0.1`, a private network, or a
  cluster-internal address) and never to a public interface. It must differ from the HTTP, OTLP/HTTP
  and gRPC addresses.
- **Docker.** The image exposes port 9464, which is used only when `CETRA_METRICS_ADDR` is set:

  ```bash
  docker run ... -e CETRA_METRICS_ADDR=0.0.0.0:9464 -p 127.0.0.1:9464:9464 ghcr.io/cetra-ai/cetra:<version>
  ```

- **docker-compose.** Set `CETRA_METRICS_ADDR=0.0.0.0:9464` in `cetra.env`. The compose file publishes
  no metrics port by default: add one, bound to `127.0.0.1`, in a `compose.override.yaml`.
- **Helm.** `metrics.enabled: true` sets `CETRA_METRICS_ADDR=0.0.0.0:9464`, adds a `metrics` container
  port, and adds the scrape annotations `prometheus.io/scrape`, `prometheus.io/port` (9464) and
  `prometheus.io/path` (`/metrics`) to the pod. The chart creates no Service port and no
  `ServiceMonitor`: Prometheus scrapes the pod (for example through pod discovery, or
  `kubectl port-forward` for a quick look).

**The label rule.** Labels take values from fixed sets in the code only (a route template, a method,
a status class, a table, an outcome, a channel kind). No metric carries an org, project, user, API key,
email address, model, URL, host or evaluator name, so per-tenant numbers stay in the usage API.

Counter names end in `_total`. Durations are histograms in seconds.

### The metrics

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `cetra_build_info` | gauge (always 1) | `version` | the server's version |
| `cetra_restore_mode` | gauge | | 1 when the server runs in restore mode |
| `cetra_http_requests_total` | counter | `route`, `method`, `status` (`2xx`…`5xx`) | responses sent, by route template; `route="other"` for unmatched paths and the UI assets |
| `cetra_http_request_duration_seconds` | histogram | `route`, `method` | request duration; the query endpoints' latency is their route here |
| `cetra_ingest_requests_total` | counter | `protocol` (`otlp_http`, `otlp_grpc`), `outcome` (`ok`, `invalid`, `unauthorized`, `too_large`, `rate_limited`, `quota`, `unavailable`, `disk_full`) | OTLP ingest requests and their outcome |
| `cetra_ingest_spans_total` | counter | `protocol` | spans accepted by OTLP ingest |
| `cetra_ingest_bytes_total` | counter | `protocol` | body bytes of accepted OTLP requests |
| `cetra_storage_memtable_bytes` | gauge | | accounted bytes of the memtables |
| `cetra_storage_memtable_hard_limit_bytes` | gauge | | the memtable hard limit; ingestion stops at it |
| `cetra_storage_failed` | gauge (0/1) | `check` (`spans_wal`, `scores_wal`, `objects_sync`, `catalog`) | a sticky storage failure is set (a restart clears it) |
| `cetra_storage_disk_free_bytes` | gauge | | least free bytes of the data directory and the local object root, at the last measurement |
| `cetra_storage_disk_min_free_bytes` | gauge | | the free-space floor below which ingestion is refused |
| `cetra_storage_wal_bytes` | gauge | `table` (`spans`, `scores`) | WAL segment bytes on disk |
| `cetra_storage_flushes_total` | counter | `table`, `outcome` (`ok`, `failed`) | memtable flushes |
| `cetra_storage_flush_seconds_total` | counter | `table` | wall time of committed flushes; divide the rate by the flush rate for the mean |
| `cetra_storage_compactions_total` | counter | `outcome` (`ok`, `failed`, `deferred`, `conflict`) | compaction jobs |
| `cetra_storage_compaction_bytes_written_total` | counter | | Parquet bytes written by committed compactions |
| `cetra_storage_gc_objects_deleted_total` | counter | | obsolete data files and staged objects (including staged media uploads); media blobs collected by media GC are not counted |
| `cetra_storage_read_cache_hits_total`, `cetra_storage_read_cache_misses_total` | counter | | S3 read cache blocks served from disk or fetched; stay at 0 in local mode |
| `cetra_storage_read_cache_bytes` | gauge | | bytes of the read cache on disk |
| `cetra_eval_jobs` | gauge | `status` (`queued`, `running`) | evaluation jobs in `meta.db`, refreshed at most every 10 s |
| `cetra_eval_jobs_finished_total` | counter | `outcome` (`done`, `failed`, `skipped`) | evaluation jobs that reached a final status |
| `cetra_eval_run_duration_seconds` | histogram | `kind` (`llm_judge`, `webhook`, `check`) | duration of one evaluator run |
| `cetra_alert_evaluations_total` | counter | `outcome` (`ok`, `failed`) | alert rules evaluated in each pass |
| `cetra_alert_deliveries_total` | counter | `channel` (`email`, `webhook`, `slack`), `outcome` (`sent`, `failed`) | alert delivery attempts |
| `cetra_emails_total` | counter | `outcome` (`sent`, `failed`, `refused`) | emails by the mailer; `refused` is restore mode |
| `process_resident_memory_bytes`, `process_open_fds`, `process_start_time_seconds` | gauge | | the server process (Linux; no `cetra_` prefix) |

Two things to know when reading them:

- HTTP metrics count only the final response of a request (2xx to 5xx).
- HTTP ingest answers 401 and 403 are counted in `cetra_http_requests_total`, not in
  `cetra_ingest_requests_total`. The `unauthorized` ingest outcome is for gRPC.

## Alert examples

These are examples for Prometheus or Alertmanager. Tune the thresholds to your load.

```promql
# Storage failure: a restart is needed (cetra_storage_failed is 1 for the failed check)
cetra_storage_failed == 1

# Free disk is below twice the ingestion floor: act before ingestion stops
cetra_storage_disk_free_bytes < 2 * cetra_storage_disk_min_free_bytes

# Flushes are failing
rate(cetra_storage_flushes_total{outcome="failed"}[10m]) > 0

# The evaluation queue is backing up
cetra_eval_jobs{status="queued"} > 1000

# More than 5 % of requests end in a server error
sum(rate(cetra_http_requests_total{status="5xx"}[5m])) / sum(rate(cetra_http_requests_total[5m])) > 0.05
```

Alert on `/healthz` and `/readyz` from outside too, with the probes of your platform or any HTTP
checker. A `/readyz` failure does not show in these metrics: `cetra_storage_disk_free_bytes` covers the
disk, and a bucket outage shows as failed flushes.
