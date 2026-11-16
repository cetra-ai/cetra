# N M3: restore mode, `/metrics` and `/readyz` (design and plan)

This is track N's third milestone. D180 and D182 (`docs/mvp-decisions.md`) give N three pieces of work:

- **Restore mode** (Q's request R18; D180). A config switch under which the server serves reads and does no outbound work: no alert deliveries, webhooks, judge calls, emails or identity-provider calls.
  - Q's restore drills start copies of cells. Those copies hold live channel and LLM-connection secrets (cetra-control `docs/00-design.md`, D-Q-12).
  - Q's phase Q2.25 needs restore mode before a cell holds customer data.
- **`/metrics`** (Q's request R11; D38, D180). Prometheus metrics on a separate admin port, for Q's M6 monitoring.
- **`/readyz` as spec 12 describes it** (D182). It adds three checks:
  - the object store is reachable (the result is cached for 10 s);
  - free disk is above the threshold where ingestion is refused;
  - the storage failure flags of D-R-6 are clear.

The decisions are D-N-25 to D-N-36 below. They are written so the user can override any of them; § Decisions marks which ones were the planner's choice.

This file holds the design and the list of phases. One agent implements each phase file (`01-…` to `08-…`) in one go, using only this file and its own phase file. Each phase ends with `just check` green and one commit. § Phases says which phases can run in parallel.

## Read first (every phase)

- `AGENTS.md`, all of it.
- This file, all of it. Then your own phase file.
- `docs/spec/12-operations.md` § Process lifecycle, § Self-monitoring and § Backups.
- `docs/spec/09-api.md`: the `/healthz` and `/readyz` rows and § CLI.
- `docs/mvp-decisions.md`: D38, D166, D180 and D182; § Track N, D-N-17 to D-N-36; § Track R, D-R-6.

## What exists (checked 2026-11-15 on `main` at `09eb59e`)

### `/readyz`

`crates/server/src/http/health.rs` serves `/readyz`. The check works as follows:

- It pings `meta.db` (`state.meta.ping()`).
- It reads `state.storage.health()` (`StorageHealth`, `crates/storage/src/lib.rs`) and fails on `spans_wal_failed` or `scores_wal_failed`.
- On failure it answers 503 with a `busy` problem (`ProblemCode::Busy`) whose detail lists the failed checks, e.g. `"meta.db, spans WAL"`.
- On success it answers `200 {"status":"ready"}`.

The module doc says the object store and disk checks "arrive with the tracks that own them".

`StorageHealth` already has `objects_sync_failed` and `catalog_failed` (track R's M2, D-R-6). `/readyz` does not read them yet.

### Free disk

`crates/storage/src/disk.rs` has a `Disk` monitor:

- It measures free space every second (`statvfs`) on the data directory and the local object root (`lib.rs`, `disk_paths`). It also measures once at open, so `low()` is right from the start.
- `Disk::low()` is `free() < min_free`, and `storage.min_free_bytes` defaults to 2 GiB. Ingestion is refused while `low()` is true (spec 03 §10).
- `Disk` is `pub(crate)`, so the server cannot see it.

### Object store

`crates/storage/src/objects.rs` holds `Objects`, which owns `store: Arc<dyn ObjectStore>` and serves both local and S3 mode. Nothing outside the storage crate can probe the store.

### Health probes in the packaging

| Where | Probe |
|---|---|
| Helm `statefulset.yaml` | `startupProbe` and `livenessProbe` on `/healthz`, `readinessProbe` on `/readyz` |
| Docker image and compose | `healthcheck: cetra health`, which runs `GET /readyz` (`crates/cli/src/main.rs`) |

### Outbound calls

Every outbound call goes through one of five places. Restore mode closes all five.

| Choke point | Built in | Used by |
|---|---|---|
| `cetra_eval::EgressClient`: SSRF-guarded, no proxy, no redirects (D166) | `serve.rs`: `EvalRunner::new(EgressPolicy { allow_private_network })` | LLM judges and webhook evaluators (`eval/dispatch.rs`, `eval/execute.rs`, `eval/test_run.rs`); alert webhook and Slack deliveries (`alerts/deliver.rs`, through `eval_runner.egress()`, D-K-11); the connection test (`routes/connections.rs`); the channel test (`routes/channels.rs`) |
| `cetra_eval::identity_http_client(policy)`: same resolver | `ee/org_sso/mod.rs` | per-org OIDC providers: login (`ee/org_sso/login.rs`) and the provider check of `put_org_sso_provider` (`ee/org_sso/settings.rs`, discovery). `spawn_org_sso` (`ee::org_sso::refresh`) only reloads from `meta.db` and calls nothing. |
| `crate::auth::oidc::identity_client()`: plain reqwest, operator-configured issuers | `serve.rs`: `Providers::from_config` | GitHub, Google and generic OIDC login (`routes/oauth.rs`, `routes/auth.rs`); discovery is lazy, at the first login |
| `ee::org_sso::dns::TxtResolver` (hickory DNS) | `serve.rs`: `OrgSso::new(…, TxtResolver::system())` | DNS TXT lookups of `post_org_sso_domain_verify` (`ee/org_sso/settings.rs`). `TxtResolver::Unconfigured` exists and looks nothing up. |
| `crate::email::Mailer` (lettre SMTP) | `serve.rs`: `Mailer::from_config`, `None` without `smtp_url` | alert emails (`alerts/deliver.rs`, `send_now`); invites and password reset (`send_in_background`); usage notices (`entitlements/notices.rs`); the SMTP test (`routes/admin.rs`, `send_now`) |

Nothing else in `crates/server`, `crates/eval` or `crates/otel` opens an outbound connection. A review checked this on 2026-11-15: no tonic client, no URL fetching in `otel` or ingest, no call at startup. The one other network peer is the S3 object store, which is storage, not outbound work (see § Restore mode › Objects). The background loops that use them are started in `serve.rs` (`bind`):

- `alerts::spawn_loops`: the evaluation, delivery and history-prune loops;
- `eval::spawn`: the dispatcher, janitor, prune, flusher, sweeper, backfill runner and budget ticker;
- `spawn_notices`: usage notices, which send email.

### Metrics

There is no Prometheus code and no metrics dependency. `routes/metrics.rs` (`MetricsCache`) is the product's metrics API (`/api/v1/…/metrics`, spec 05) and is unrelated to this milestone. Counters that already exist:

- `StorageHealth::memtable_bytes` and `memtable_hard_limit`;
- `Storage::read_cache_stats()` (`hits`, `misses`, `bytes`, `blocks`);
- `usage::Usage`, per-org units, which must **not** become labelled metrics (§ Metrics, label rule).

## Design

### Restore mode (D-N-25 to D-N-27)

**Switch.** A top-level config key, `restore_mode = true`, or `CETRA_RESTORE_MODE=true` through the usual env mapping. It defaults to `false`. There is no `serve` flag: one way to set it, and Q's drill script already supports a config key (Q2.25 § Precondition).

**What it turns off.** All five choke points are closed, the loops that would only fail are not started, and the storage deletes no object:

1. **`EgressPolicy` gets `restore_mode: bool`** (`crates/eval/src/egress.rs`).
   - When it is set, `EgressClient::post` returns the new `EgressError::RestoreMode` before doing anything, `EgressPolicy::allows_url` returns `false`, and `GuardedResolver` resolves no name.
   - `identity_http_client(policy)` therefore yields a client that cannot connect, and every caller already checks `allows_url` first.
   - The server builds both `EvalRunner` and the org-SSO clients with `restore_mode: config.restore_mode`.
   - `EgressError::class()` puts `RestoreMode` in the `Permanent` class, so nothing retries it.
2. **Operator identity providers are not built.** In restore mode `Providers::from_config` is not called and `Providers` is empty: no GitHub, Google or OIDC button, and `GET /api/auth/config` lists no `providers`. Password login and API keys keep working (D-N-27).
   - Users who cannot use a password cannot sign in: members of orgs that enforce SSO (`enforces()` in `auth/session.rs` and `auth/principal.rs`), and everyone when `auth.disable_password` is set. They use API keys.
3. **DNS lookups are off.** `OrgSso` is built with `TxtResolver::Unconfigured` instead of `TxtResolver::system()`.
4. **The mailer gets a refusing transport.** `Transport::Off` replaces SMTP when `restore_mode` is set and `smtp_url` is configured:
   - `send_now` returns the new `MailError::RestoreMode` (label `restore_mode`).
   - `send_in_background` logs `email not sent: restore mode` with the message kind and drops the email.
   - Without `smtp_url` the mailer stays `None`, as today.
5. **Loops not started:**
   - the alert **evaluation** and **delivery** loops (the history-prune loop still runs);
   - the eval **dispatcher** and **backfill runner** (the janitor, prune, flusher, sweeper and budget ticker still run: they only touch `meta.db`);
   - `spawn_notices` (usage notices send email);
   - the org-SSO domain recheck (`spawn_org_sso_recheck`, a DNS TXT lookup per pass).

   `spawn_org_sso` keeps running: it only reloads from `meta.db`.

   Queued eval jobs and pending deliveries stay as they are in `meta.db`. A drill copy is thrown away, and a real restore runs without restore mode, so they then proceed normally.
6. **Objects: the storage deletes no object (D-N-34).** The server opens the storage with object deletion off: phase 01's `StorageConfig::delete_objects`, which is not a TOML key.
   - `Objects::delete` and `delete_len` then fail with `ObjectError::DeletionOff`. Every caller already keeps its catalog row and retries later, so GC, media collection and project drops remove nothing, and a later server without restore mode collects normally.
   - A project drop stays `dropping`.
   - Flushes, compaction, deletions and retention still write new objects and mark old ones obsolete in the catalog.
   - So a drill copy whose config still points at the live bucket by mistake can only **add** objects to it. The live server's catalog does not know them, and its staged-object GC removes them after an hour. It can never delete a live object.
   - `gc-orphans` (`Maintenance`) always deletes, even with `restore_mode` in the config.
   - The drill should still use a copy of the bucket, as Q's drill does (`<BUCKET>-drill`). The restore procedure says so.

**What keeps running:** every read, ingestion, score writes, deletions and retention (in the catalog), compaction, usage counters, the admin socket (backups), live updates and the UI. Restore mode is not read-only: a restored copy must be able to re-apply deletions (D-M-9) and accept the drill's probe traffic.

**User-triggered outbound calls** answer `503` with a new problem code, `restore_mode` ("this server runs in restore mode: outbound calls are off"). A single helper, `crate::http::restore::refuse_outbound(&state, what) -> Result<(), ApiError>`, returns the new `ApiError::RestoreMode { what }` and is called first in each such handler (D-N-26). § 04 lists the handlers:

- channel test;
- connection test;
- evaluator test run;
- manual eval runs ("run on this trace");
- backfill create;
- the org-SSO provider check and the domain verification (a DNS TXT lookup through `OrgSso`'s resolver);
- the SMTP test;
- the OAuth and OIDC login start and callback, and the org-SSO login.

The choke points above would refuse these calls anyway. The helper makes the answer clear instead of a timeout or a "connect failed".

**How to see it is on** (Q2.25 asks for a documented check):

- at startup, one `warn` line: `restore mode: outbound work is off (alerts, webhooks, judges, email, identity providers)`;
- `/readyz` answers `{"status":"ready","restore_mode":true}`; the field is present only when the mode is on;
- the metric `cetra_restore_mode` is `1`.

### `/readyz` (D-N-30)

The checks run in this order. Each failure adds a name to the 503 problem's detail.

| Check | Source | Failure name |
|---|---|---|
| `meta.db` answers | `state.meta.ping()` (exists) | `meta.db` |
| spans and scores WAL | `StorageHealth::{spans,scores}_wal_failed` (exists) | `spans WAL`, `scores WAL` |
| local object fsync | `StorageHealth::objects_sync_failed` | `object fsync` |
| catalog | `StorageHealth::catalog_failed` | `catalog` |
| free disk | `StorageHealth::disk_low` (new, phase 01) | `free disk` |
| object store reachable | `Storage::probe_objects()` (new, phase 01), cached 10 s by the server | `object store` |

**The probe (D-N-36)** goes through `Objects::raw_store()`, the prefixed store without the read cache.
- It HEADs the marker `_probe/readyz`. If that fails for any reason, it PUTs the marker (0 bytes). The store is reachable when either call succeeds.
- Why a marker: S3 answers a HEAD of a missing key with 403, not 404, when the credentials lack `s3:ListBucket`. A HEAD alone of a missing key would then fail forever with least-privilege credentials. With the marker, once it exists, every probe is one HEAD that needs only `s3:GetObject`; the first one needs `s3:PutObject`, which the server has anyway.
- `_probe/` is outside `tables/` and `media/`, so GC, `gc-orphans` and backups never see it.
- `object_store`'s own `RetryConfig` retries inside each call, so the server's **2 s timeout** bounds the whole probe, and a timeout means unreachable.
- In local mode the probe goes through the same `ObjectStore`.

**The cache.** `ReadyCache` in `http/health.rs` keeps the last outcome and its `Instant`.
- A result younger than 10 s is reused, success or failure.
- One `tokio::sync::Mutex` makes concurrent probes wait for one request instead of each sending their own.

**The body.** `200 {"status":"ready"}` (plus `restore_mode`) or the 503 `busy` problem, as today. The detail names checks only, never a key, a path or an error text.

**Liveness (D-N-35).** The storage failure flags are sticky: `spans_wal_failed`, `scores_wal_failed`, `objects_sync_failed` and `catalog_failed` stay set until a restart (D-R-6). A restart is the remedy: WAL replay and a fresh catalog connection.
- `/healthz` therefore also fails (503 `busy`, detail `restart needed: <checks>`) when one of them is set. It does no I/O: `StorageHealth` is counters only.
- Helm's liveness probe, already on `/healthz`, then restarts the pod after 6 × 10 s, instead of leaving a single-replica StatefulSet out of its Service forever.
- `meta.db`, free disk and the object store are not sticky. They stay readiness-only: a restart would not fix them, and a full disk must not cause a restart loop.

**What each environment does:**
- **Kubernetes.** A full disk or an unreachable bucket takes the pod out of the Service until it recovers. The UI is unreachable through the Service meanwhile, which D182 accepts. `kubernetes.md` says to free space through `kubectl exec` or a bigger volume, and to alert on `cetra_storage_disk_free_bytes`.
- **Docker.** Its healthcheck (`cetra health` → `/readyz`) marks the container `unhealthy`, which restarts nothing under plain Docker or compose.

`docs/self-hosting/monitoring.md` says all of this. Nothing in `deploy/` changes for the probes.

### `/metrics` (D-N-28, D-N-29)

**The port.** A new top-level key, `metrics_addr`, or `CETRA_METRICS_ADDR`.
- It defaults to unset (off), and `""` also means off, like `grpc_addr`.
- It must differ from `http_addr`, `otlp_http_addr` and `grpc_addr`; it is validated like them.
- The listener serves exactly `GET /metrics` and answers 404 for anything else.
- There is no authentication: the docs say to bind it to a private interface or keep it inside the cluster.
- It is bound with the other sockets, before any server starts, and stopped first on shutdown.

**The library.** `prometheus-client` **0.26.0** (`cargo info prometheus-client`, 2026-11-15; Apache-2.0 OR MIT), the official Prometheus Rust client.
- It encodes the OpenMetrics text format with `Content-Type: application/openmetrics-text; version=1.0.0; charset=utf-8`, which Prometheus scrapes natively.
- It is declared once in `[workspace.dependencies]` and used by `cetra-server` only.

**The registry.** It lives in a new module, `crates/server/src/telemetry.rs`, chosen so it does not clash with `routes::metrics`.
- `pub(crate) struct Telemetry` holds the counters, gauges and histograms as fields (`prometheus_client::metrics::family::Family` where labelled) and a `Registry` built once in `Telemetry::new()`.
- **One root `Registry::default()`.** The `process_*` gauges are registered on the root; every `cetra_*` metric is registered on `root.sub_registry_with_prefix(PREFIX)`. Encoding the root once gives one valid document with a single `# EOF`.
- **prometheus-client rules** (0.26.0; checked in its source):
  - A counter is registered **without** `_total`: `register("http_requests", …)` renders `cetra_http_requests_total`. `Registry::register` debug-asserts that a counter name does not end in `_total`.
  - A unit comes from the name or from `register_with_unit`, never both. Use names that already end in `_seconds`/`_bytes` and plain `register`.
  - Build info is a `Family<VersionLabel, Gauge>` named `build_info`, set to 1. `Info` would render `cetra_build_info_info`.
  - Labelled histograms are `Family<L, Histogram, fn() -> Histogram>`, built with `Family::new_with_constructor(fn)`.
  - `Counter<f64, AtomicU64>` for float counters.
  - `#[derive(EncodeLabelValue)]` writes the variant's identifier as is: name variants like the values with `#[allow(non_camel_case_types)]`, e.g. `otlp_http`, `GET` (`#[allow(clippy::upper_case_acronyms)]`). A value that is not an identifier (`2xx`) needs a hand-written `impl EncodeLabelValue`.
  - A family series appears only after its first use. Gauges that must show `0` (e.g. `cetra_eval_jobs{status="queued"}`) are set on every render.
- `AppInner` gets `telemetry: Telemetry`.
- Gauges that read the state (storage health, disk, read cache, eval queue depth, process) are refreshed by `Telemetry::render(&state)` before it encodes, not by a background task.
- The eval queue count is one `SELECT status, COUNT(*) FROM eval_jobs WHERE status IN ('queued','running') GROUP BY status`. Its result is cached for 10 s, so scrapes every second cannot load `meta.db`.

**The prefix.** Every name starts with `cetra_`, written once as `const PREFIX: &str = "cetra"` and passed to `Registry::with_prefix`, so the product rename (D46) touches one line.

**Label rule (D-N-29).** Labels take values from fixed, code-defined sets only:
- a route template, a protocol, a table name, an outcome, a channel kind, a status class;
- never an org, project, user, API key, email, model, URL, host, evaluator name or trace ID.

Per-tenant numbers stay in the usage API (P) and the control plane. This keeps cardinality bounded and tenant names out of a monitoring system that Q's operators and their tools read.

**The metrics.** Rendered counter names end in `_total` (registered without it, above); durations are histograms in seconds.

| Metric | Type | Labels | Source |
|---|---|---|---|
| `cetra_build_info` | gauge = 1 | `version` | `env!("CARGO_PKG_VERSION")` |
| `cetra_restore_mode` | gauge | — | config |
| `cetra_http_requests_total` | counter | `route`, `method`, `status` (`2xx`…`5xx`) | middleware (phase 06) |
| `cetra_http_request_duration_seconds` | histogram | `route`, `method` | middleware |
| `cetra_ingest_requests_total` | counter | `protocol` (`otlp_http`, `otlp_grpc`), `outcome` (`ok`, `invalid`, `unauthorized`, `too_large`, `rate_limited`, `quota`, `unavailable`, `disk_full`) | ingest handlers |
| `cetra_ingest_spans_total` | counter | `protocol` | ingest handlers, accepted spans |
| `cetra_ingest_bytes_total` | counter | `protocol` | request body size, accepted requests |
| `cetra_storage_memtable_bytes` / `_memtable_hard_limit_bytes` | gauge | — | `StorageHealth` |
| `cetra_storage_failed` | gauge (0/1) | `check` (`spans_wal`, `scores_wal`, `objects_sync`, `catalog`) | `StorageHealth` |
| `cetra_storage_disk_free_bytes` / `_disk_min_free_bytes` | gauge | — | `StorageHealth` (phase 01) |
| `cetra_storage_wal_bytes` | gauge | `table` (`spans`, `scores`) | `StorageStats` (phase 01) |
| `cetra_storage_flushes_total` | counter | `table`, `outcome` (`ok`, `failed`) | `StorageStats` |
| `cetra_storage_flush_seconds_total` | counter | `table` | `StorageStats` (sum; rate ÷ flushes gives the mean) |
| `cetra_storage_compactions_total` | counter | `outcome` (`ok`, `failed`, `deferred`, `conflict`) | `StorageStats` |
| `cetra_storage_compaction_bytes_written_total` | counter | — | `StorageStats` |
| `cetra_storage_gc_objects_deleted_total` | counter | — | `StorageStats` (obsolete data files and staged objects, including staged media uploads; media blobs collected by media GC are not counted) |
| `cetra_storage_read_cache_hits_total` / `_misses_total` | counter | — | `read_cache_stats()` (absent in local mode) |
| `cetra_storage_read_cache_bytes` | gauge | — | `read_cache_stats()` |
| `cetra_eval_jobs` | gauge | `status` (`queued`, `running`) | `meta.db` count, cached 10 s |
| `cetra_eval_jobs_finished_total` | counter | `outcome` (`done`, `failed`, `skipped`, the `eval_jobs` final statuses) | dispatcher |
| `cetra_eval_run_duration_seconds` | histogram | `kind` (`llm_judge`, `webhook`, `check`) | dispatcher |
| `cetra_alert_evaluations_total` | counter | `outcome` (`ok`, `failed`) | evaluation loop |
| `cetra_alert_deliveries_total` | counter | `channel` (`email`, `webhook`, `slack`), `outcome` (`sent`, `failed`) | delivery loop |
| `cetra_emails_total` | counter | `outcome` (`sent`, `failed`, `refused`) | `Mailer` |
| `process_resident_memory_bytes`, `process_open_fds`, `process_start_time_seconds` | gauge | — | Linux: `VmRSS` of `/proc/self/status`, the entries of `/proc/self/fd`, the start time; absent elsewhere (no prefix: Prometheus convention) |

**Query latency** is not a metric of its own. The query endpoints' latency is `cetra_http_request_duration_seconds` for their routes (`/api/v1/projects/{project}/traces`, `…/sql`, `…/metrics`), which is what an operator watches. A per-surface storage histogram would need storage hooks for no new information.

The **histogram buckets** are prometheus-client's `exponential_buckets(0.005, 2.0, 12)`: 5 ms to about 10 s for HTTP, and `exponential_buckets(0.05, 2.0, 12)` for eval runs (50 ms to about 100 s).

**`route`.**
- It is axum's `MatchedPath` (the template, e.g. `/api/v1/orgs/{org}/projects/{project}/traces`), read by one middleware added in `http/mod.rs` `finish()`, which already builds both the main and the OTLP HTTP router. `log_request` already reads `MatchedPath` there.
- Requests that match no route, including UI assets and the SPA fallback, get `route="other"`.
- gRPC requests are counted by the ingest counters only.

### Storage API needed from track R (phase 01; D-N-31)

`crates/storage` is R's area (`docs/roadmap.md`, track R). Phase 01 is the only phase that edits it. R reviewed it on 2026-11-15 and asked for six changes, which phase 01 includes; N implements it after R's stacked PRs are merged. Items 1–3 only add read access. Item 4 adds a switch that the server sets only in restore mode.

1. `StorageHealth` gets two fields:
   - `disk_low: bool`, from `Disk::low()`;
   - `disk_free_bytes: u64` and `disk_min_free_bytes: u64`, from `Disk::free()` and `min_free`.
2. `pub async fn probe_objects(&self) -> Result<(), ProbeError>`. It HEADs the marker `_probe/readyz` through `Objects::raw_store()` and, if that fails, PUTs it (D-N-36). `ProbeError` is a `thiserror` enum with one variant, `Unreachable`, whose text names no key. The caller bounds it with a timeout.
3. `pub async fn stats(&self) -> StorageStats`, plain counters read with `Relaxed` loads:
   - `wal_bytes: [u64; 2]` (spans, scores; the bytes of the live segments);
   - `flushes_ok`, `flushes_failed` and `flush_nanos`, per table;
   - `compactions_ok`, `compactions_failed`, `compactions_deferred` and `compactions_conflict`;
   - `compaction_bytes_written`;
   - `gc_objects_deleted`.

   The counters are `AtomicU64` fields of one `Counters` struct. An `Arc<Counters>` hangs on the `Committer`, which flush, compaction and GC already share, so no task signature changes. Phase 01 lists the exact sites.
4. **`StorageConfig::delete_objects: bool`**, `#[serde(skip)]` (not a TOML key; `StorageConfig` is `deny_unknown_fields`), default `true` (D-N-34). When `false`, `Objects` refuses every deletion with `ObjectError::DeletionOff`, and callers keep their rows. `Maintenance` (`gc-orphans`) always deletes.

Track R reviewed these on 2026-11-15. N implements phase 01, after R's stacked PRs are merged, and keeps the probe and the counters out of R's model, stress and soak harnesses.

   The server sets it to `!config.restore_mode` before `Storage::open` (phase 02). Spec 03 § 5 gets one sentence.

### What M3 does not do

- **No OTel self-traces.** Spec 12 lists them as opt-in; they are not needed by Q and stay deferred under D38.
- **No `ServiceMonitor`** (it needs the Prometheus Operator's CRDs). The chart only exposes the port and optional scrape annotations (D-N-32).
- **No UI change for restore mode.** The UI shows the `restore_mode` problem's message as it shows any problem. A banner can come later if self-hosters use restore mode outside drills.
- **No read-only mode.** Restore mode refuses outbound work and object deletion, not writes (above).
- **No per-tenant metrics** (D-N-29).

## Decisions

They are in `docs/mvp-decisions.md` § Track N from this plan's PR. "(planner)" marks a choice made in this plan that the user may override. Phase 08 rewrites the mark as "(user)" for each one the coordinator confirms.

| ID | Decision |
|---|---|
| D-N-25 | **Restore mode is a config key** (`restore_mode`, `CETRA_RESTORE_MODE`), not a `serve` flag. It closes the five outbound choke points (egress policy, org-SSO clients, operator identity providers, the org-SSO DNS resolver, the mailer). It also leaves unstarted the alert evaluation and delivery loops, the eval dispatcher and backfill runner, and usage notices. It shows in a startup `warn` line, in `/readyz`'s `restore_mode` field and in `cetra_restore_mode`. Reads, ingestion, deletions, retention, compaction and backups keep running. (planner) |
| D-N-26 | **User-triggered outbound calls in restore mode answer 503 `restore_mode`**, through one helper called first in each handler, on top of the choke points. (planner) |
| D-N-27 | **No external sign-in in restore mode**: operator providers are not built, and org-SSO login is refused, because the token exchange would reach a live IdP with the cell's client secret. Password login and API keys still work. Members of SSO-enforced orgs, and everyone under `auth.disable_password`, use API keys. (planner) |
| D-N-28 | **`/metrics` on `metrics_addr`** (off by default), no authentication, `prometheus-client` 0.26.0, OpenMetrics text. (planner, within D38's "separate admin port") |
| D-N-29 | **The metric set of § `/metrics`, and the label rule**: labels from fixed code-defined sets only; never an org, project, user, key, email, model, URL, host or evaluator name. Query latency is the HTTP route histogram. (planner) |
| D-N-30 | **`/readyz`** checks `meta.db`, both WALs, object fsync, the catalog, free disk, and the object-store probe of D-N-36. The probe has a 2 s timeout and is cached 10 s with single flight. (planner, implements D182) |
| D-N-31 | **Storage additions, reviewed by R**: `StorageHealth::{disk_low, disk_free_bytes, disk_min_free_bytes}`, `Storage::probe_objects`, `Storage::stats() -> StorageStats` (counters on the `Committer`), and `StorageConfig::delete_objects` (D-N-34). Reviewed by R on 2026-11-15; N implements them after R's stacked PRs. |
| D-N-32 | **Packaging**: the image `EXPOSE`s 9464, the conventional port of OpenTelemetry's Prometheus exporter. The chart gets `metrics.enabled` (default `false`), which sets `CETRA_METRICS_ADDR=0.0.0.0:9464`, adds a `metrics` container port and adds `metrics.podAnnotations` (`prometheus.io/scrape`, `port`, `path`) for pod scraping. It gets no Service port, because a `LoadBalancer` Service would publish the unauthenticated port, and no `ServiceMonitor`. Compose documents the key in a comment. (planner) |
| D-N-33 | **D38 settled for `/metrics`**: spec 12 § Self-monitoring describes `/metrics` and `/readyz` as built; OTel self-traces stay deferred. |
| D-N-34 | **Restore mode deletes no object.** The storage opens with `delete_objects = false`, so `Objects` refuses every deletion and callers keep their catalog rows: GC, media collection and project drops remove nothing until a later server. A drill copy pointed at the live bucket by mistake can only add objects, which the live server's staged-object GC removes, and never deletes a live one. `gc-orphans` still deletes. Drills still use a bucket copy. (planner; R's design, 2026-11-15) |
| D-N-35 | **`/healthz` fails on the sticky storage failures** (`*_wal_failed`, `objects_sync_failed`, `catalog_failed`, D-R-6), so Kubernetes' liveness probe restarts the pod. `meta.db`, free disk and the object store stay readiness-only, because a restart would not fix them. (planner) |
| D-N-36 | **The object-store probe uses a marker.** `/readyz` HEADs `_probe/readyz` and, when that fails, PUTs it (0 bytes); reachable when either succeeds. A HEAD of a missing key alone would fail forever on S3 with credentials lacking `s3:ListBucket` (403 instead of 404). No write at open, so storage tests that list every key are unchanged. **For the user's confirmation** (track R's recommendation; the alternative is to require `s3:ListBucket` in the docs). |

## Phases

| Phase | File | Commit | Needs | Area |
|---|---|---|---|---|
| 01 | [01-storage-api.md](01-storage-api.md) | `Expose disk, object-store probe and counters from storage` | R's stacked PRs merged | `crates/storage` (R reviewed it) |
| 02 | [02-readyz.md](02-readyz.md) | `Check the object store, free disk and storage flags in /readyz` | 01, 03 | server `http/health.rs` (`/readyz`, `/healthz`), specs 09 and 12 |
| 03 | [03-restore-mode.md](03-restore-mode.md) | `Add restore mode` | — | `crates/eval/src/egress.rs`, server config, serve, email, ee/org_sso |
| 04 | [04-restore-handlers.md](04-restore-handlers.md) | `Refuse user-triggered outbound calls in restore mode` | 03 | server routes, `http/restore.rs`, `ProblemCode` |
| 05 | [05-metrics-endpoint.md](05-metrics-endpoint.md) | `Serve Prometheus metrics on metrics_addr` | 03 (`cetra_restore_mode` reads `config.restore_mode`) | root `Cargo.toml`, server `telemetry.rs`, config, serve |
| 06 | [06-metrics-instrumentation.md](06-metrics-instrumentation.md) | `Count HTTP, ingest, eval, alert and email events` | 05 | server http middleware, ingest, eval, alerts, email |
| 07 | [07-storage-metrics.md](07-storage-metrics.md) | `Export the storage counters as metrics` | 01, 05 | server `telemetry.rs` |
| 08 | [08-packaging-docs.md](08-packaging-docs.md) | `Document restore mode, /metrics and /readyz` | all | `deploy/`, `docs/`, CHANGELOG, decisions, roadmap |

**Parallel work.** Two chains can run at the same time, each in its own worktree:

- 01 (after R's stacked PRs are merged);
- 03 → then 04 and 05 in parallel → 06 after 05.

02 needs 01 and 03; it also wires the object-deletion switch of D-N-34. Until 02 lands, restore mode is complete except that the storage still deletes objects. 07 needs 01 and 05.

Shared files to watch:

- **`AppInner`** (`http/mod.rs`, the `serve.rs` literal and `TestApp`) gets one field from 02 (`ready`) and one from 05 (`telemetry`). Whichever lands second rebases.
- **`serve.rs`** is touched by 02 (one field), 03 (loops, constructors) and 05 (the listener). 05 starts after 03, so only 02's one line can conflict.
- **`telemetry.rs`** is created by 05, then extended by 06 and 07. 06 and 07 can run in parallel, but both edit `render`; the second to land rebases.
- **`http/mod.rs`** is edited by 02, 04 and 05, and **`ServerConfig` literals** (`test_support/mod.rs` `default_config` and `config`, `ingest/fixtures_tests.rs`) by 03 and 05.
- **`docs/spec/12-operations.md`** is edited by 02, 03, 05 and 08, in different sections.

All of these conflicts are small; the second phase to land rebases.

08 goes last.

## Conventions

- **Logs and errors** never contain a key, a URL, a host, an email address, a path inside the bucket or payload (`AGENTS.md` § Privacy). The restore-mode refusals name the kind of call (`judge`, `webhook`, `email`, `identity provider`), never its target.
- **Tests** cover the current behaviour only. Every new branch has a test in the phase that adds it.
- **No compatibility shims.** `EgressPolicy`'s new field is set by every constructor in the same phase (03).
- **`CHANGELOG.md`** gets its lines in phase 08 only, so the parallel phases don't conflict on it.
- **`just check`** before each commit. For a custom `CARGO_TARGET_DIR`, use one under `~/.cache` (never `/tmp`) and delete it when the phase is merged.

## How to verify (whole milestone)

With a release build (`just build-release`) and a local data directory:

1. **Readiness.** `cetra serve` with `metrics_addr = "127.0.0.1:9464"`. `curl -s localhost:8080/readyz` gives `{"status":"ready"}`. `curl -s localhost:9464/metrics | grep -E '^cetra_(build_info|restore_mode|storage_disk_free_bytes)'` shows the three.
2. **Ingest metrics.** Run `tools/packaging/smoke.sh`, then `cetra_ingest_spans_total{protocol="otlp_http"}` is > 0 and `cetra_http_requests_total` has a `route` label for the traces API.
3. **Disk check.** Stop, set `storage.min_free_bytes` above the free space, start: `/readyz` answers 503 with `free disk` in the detail; `/healthz` still answers 200 (free disk is not sticky, D-N-35).
4. **Object-store check (S3 mode).** With `deploy/compose/compose.s3.yaml`, stop the Garage container: within 10 s `/readyz` is 503 with `object store`; restart Garage and it is 200 again within 10 s.
5. **Restore mode.** Stop, add `restore_mode = true`, start:
   - the `warn` line appears;
   - `/readyz` has `"restore_mode":true`;
   - `cetra_restore_mode 1`;
   - `POST` the channel test of a webhook channel pointing at `http://127.0.0.1:9` gives 503 `restore_mode`, and nothing listens there so a real attempt would show a different error;
   - an alert rule that fires creates no delivery attempt;
   - password login works and the provider list is empty;
   - the log has `object deletion is off (restore mode)`, and after `storage.gc.grace` the obsolete files of a compaction are still in `objects/`.
6. `just check` and `just check-release` pass. `just helm-check` passes with `metrics.enabled=true`.
