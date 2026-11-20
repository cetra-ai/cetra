# 12 — Operations: configuration, packaging, limits, metering, backups, upgrades

## Configuration

Sources, later overriding earlier: built-in defaults → `cetra.toml` (`--config` or `CETRA_CONFIG`) → env vars (`CETRA_` + upper-snake path, e.g. `CETRA_STORAGE__OBJECT_STORE` for `storage.object_store`; common ones also have short aliases listed below). Parsed with `figment`; unknown keys are an error. The log filter always caps the `rmcp` target at `warn`, since that crate logs MCP arguments and results at `debug` (D-H-11). `CETRA_URL`, `CETRA_TOKEN` and `CETRA_PROJECT` belong to the CLI's remote commands (D-H-12) and are not config keys. `cetra admin check` validates config, data dir, encryption key, SMTP and object store connectivity.

GitHub and Google also accept the split form (`CETRA_AUTH__GITHUB__CLIENT_ID`, `CETRA_AUTH__GOOGLE__CLIENT_SECRET`, and the other scalar fields of those tables). `[[auth.oidc]]` providers are defined in TOML; only their client secret has an env form, `CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET`, with the provider's `name` upper-cased and `-` written `_` (`my-idp` → `CETRA_AUTH__OIDC__MY_IDP__CLIENT_SECRET`). It wins over `client_secret` in the file, which may then be left out. Any other `CETRA_AUTH__OIDC__*` variable, or one naming no provider of the file, is a load error (D-S-2).

| Key | Env alias | Default |
|---|---|---|
| `data_dir` | `CETRA_DATA_DIR` | `./cetra-data` |
| `public_url` | `CETRA_PUBLIC_URL` | `http://localhost:8080` |
| `http_addr` | `CETRA_HTTP_ADDR` | `0.0.0.0:8080` |
| `trusted_proxies` | `CETRA_TRUSTED_PROXIES` | empty (comma-separated CIDRs, D142) |
| `grpc_addr` | `CETRA_GRPC_ADDR` | `0.0.0.0:4317` (empty = disabled; a config key from track G M3) |
| `otlp_http_addr` | `CETRA_OTLP_HTTP_ADDR` | `0.0.0.0:4318` (empty = disabled) |
| `metrics_addr` | `CETRA_METRICS_ADDR` | unset (off). Prometheus `/metrics`; no authentication, bind it to a private address (D-N-28) |
| `restore_mode` | | `false`. No outbound work: alert deliveries, webhooks, judges, email and identity providers are off; for restore drills (D-N-25) |
| `storage.object_store` | `CETRA_OBJECT_STORE` | `file://<data_dir>/objects`. A set value is `file://<absolute path>` or `s3://<bucket>[/<prefix>]` |
| `storage.s3.region` | | unset: the `AWS_REGION` environment variable (not an alias; the loader has none) |
| `storage.s3.endpoint` | | unset (MinIO, R2, …) |
| `storage.s3.force_path_style` | | `false` |
| `storage.s3.allow_http` | | `false` |
| `encryption_key` | `CETRA_ENCRYPTION_KEY` | unset: read `<data_dir>/secret.key`, generated on the first start of a new data directory (D-I-1) |
| `license_key` | `CETRA_LICENSE_KEY` | unset: no licence, enterprise features locked (D172, D-P-7) |
| `entitlements.defaults.plan_name` | | `"Self-hosted"`: the plan name shown for orgs without a token (D-P-10) |
| `entitlements.defaults.features` | | unset: every core feature (`retention_management`); a list grants only those; `ee` names are refused (D-P-10) |
| `entitlements.defaults.<limit>` | | unset = unlimited, for each of `units_included`, `units_cap`, `ingest_spans_per_sec`, `ingest_burst`, `data_access_days`, `max_seats`, `max_projects`, `max_alert_rules`, `max_active_online_rules`, `max_annotation_queues` (D-P-13, D-AQ-10) |
| `orgs.user_create` | | `true`; `false` on a cloud cell: users cannot create orgs, the control plane does (D-P-31) |
| `orgs.create_url` | | unset: where "New organization" links when `orgs.user_create` is false (D-P-31) |
| `demo.org` | `CETRA_DEMO__ORG` | unset (no demo org). An org id: every signed-in user who is not an instance admin reads that org as an implicit viewer (08 § Demo org, D187). The org must exist and be active at startup, or startup fails |
| `demo.requests_per_minute` / `demo.user_in_flight` / `demo.org_in_flight` | | `120` / `4` / `16`: limits of implicit visitors, per user and all together (08 § Demo org) |
| `demo.sql_timeout` / `demo.sql_max_rows` | | `"10s"` / `1000`: the SQL console's caps for implicit visitors |
| `entitlements.upgrade_url` | | unset: no Upgrade button. `{org}` is replaced by the org id (D-P-15) |
| `entitlements.control_plane_provider` | | `"oidc:cloud"`: the identity provider whose subjects the control plane uses (D-P-39) |
| `entitlements.control_plane` | | `false`; `true` on a cloud cell only: the cell accepts org tokens and control-plane requests. When false, `/api/internal/v1/*` answers 404, stored org tokens are ignored and only licence keys are trusted (D-P-50) |
| `smtp_url`, `email_from` | `CETRA_SMTP_URL`, `CETRA_EMAIL_FROM` | unset (email off) |
| `auth.*` | see 08 | |
| `init.admin_email` | `CETRA_INIT_ADMIN_EMAIL` | unset |
| `init.admin_password` | `CETRA_INIT_ADMIN_PASSWORD` | unset |
| `init.org` | `CETRA_INIT_ORG` | unset |
| `init.project` | `CETRA_INIT_PROJECT` | unset |
| `init.secret_key` | `CETRA_INIT_SECRET_KEY` | unset (optional `sk-cetra-` key for headless init) |
| `ingest.max_request_bytes` | | `"10MiB"` (before and after decompression) |
| `ingest.max_spans_per_request` | | `50000` |
| `ingest.max_inflight_bytes` | | `"256MiB"` (request bytes held at once, D-G-5) |
| `ingest.max_grpc_message_bytes` | | `"32MiB"` (with the gRPC listener, track G M3) |
| `ingest.max_attribute_bytes` | | `"8MiB"` |
| `media.min_bytes` / `media.max_bytes` | | `"4KiB"` / `"32MiB"` |
| `storage.min_free_bytes` | | `"2GiB"`: ingestion is refused below this much free disk |
| `storage.wal.sync` | | `"always"` (or a duration such as `"5ms"`: fsync at most that often) |
| `storage.flush.max_bytes` / `storage.flush.max_age` / `storage.flush.upload_concurrency` | | `"256MiB"` / `"120s"` / `8` |
| `storage.wal.segment_bytes` / `storage.wal.group_max_bytes` / `storage.wal.group_max_wait` / `storage.wal.skip_corrupt` | | `"64MiB"` / `"8MiB"` / `"2ms"` / `false` |
| `storage.memtable.hard_limit` | | `"1GiB"` |
| `storage.compaction.target_file_size` / `storage.compaction.threads` | | `"512MiB"` / `max(1, cores/4)` |
| `storage.compaction.interval` / `storage.compaction.close_delay` | | `"30s"` / `"2h"` |
| `storage.compaction.size_ratio` / `storage.compaction.max_memory` | | `4` / `"1GiB"` |
| `storage.compaction.l0_files` / `storage.compaction.l0_bytes` / `storage.compaction.l0_age` | | `8` / `"512MiB"` / `"10m"` |
| `storage.gc.grace` | | `"5m"` (at most `"30d"`; the window a meta-only S3 backup stays restorable, D-N-20) |
| `storage.query.max_memory` | | `"2GiB"` |
| `storage.query.metadata_cache_bytes` | | `"512MiB"` |
| `storage.query.target_partitions` / `storage.query.trace_lookback` / `storage.query.admission_wait` | | the CPU count / `"1h"` / `"5s"` |
| `storage.query.max_memory_total` / `storage.query.initial_memory` / `storage.query.read_cache_bytes` | | unset (half the machine's memory, at most `"8GiB"`) / `"64MiB"` / unset (10% of the file system, at most `"100GiB"`; `"0B"` turns it off) |
| `storage.anchor.window` | | unset (`storage.query.trace_lookback`) |
| `storage.deletion.max_delay` | | `"24h"` |
| `eval.workers` | | `16` (1–256, D-I-6) |
| `security.allow_private_network_egress` | | `false` (D166) |
| `log.format` / `log.level` | `CETRA_LOG` | `pretty` on TTY else `json` / `info` |

The storage engine's settings form the `[storage]` section, with one sub-table per area (`wal`, `s3`, `flush`, `memtable`, `compaction`, `query`, `gc`, `deletion`, `anchor`); spec 03 names them without the `storage.` prefix. Sizes are strings with a binary unit (`"256MiB"`, `"1GiB"`) and durations are strings with a unit (`"2ms"`, `"120s"`, `"24h"`) (D90). The `[ingest]` and `[media]` sections use the same formats. S3 credentials are the standard `AWS_*` environment variables and are not config keys. The full list, with defaults, is the `StorageConfig` doc in `cetra-storage`, and the site's configuration reference, which `config::tests::the_configuration_reference_matches_the_loader` checks against the loader. Memory, disk and file-descriptor needs of the storage engine, and a profile for small machines, are in spec 03 § 10 Resource needs.

TLS: terminated by a reverse proxy in the reference setups. cetra has no `tls.*` keys and no telemetry key: the loader refuses them as unknown keys.

## Packaging

Track N builds, publishes and tests these (`docs/plans/n-packaging/`, D-N-1–D-N-16). User documentation: `website/src/content/docs/docs/self-hosting/`, served at cetra.sh/docs/self-hosting/.

| Artifact | Details |
|---|---|
| Binary | `cetra` for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (static) and `aarch64-apple-darwin` (D-N-1). Web UI embedded. Built natively per platform with `cargo auditable build --release --locked` (the workspace release profile: LTO thin, `codegen-units=1`, unwinding kept for task isolation). Shipped as `cetra-<version>-<target>.tar.gz` with `LICENSE`, `LICENSE-ELASTIC` and `THIRD-PARTY-NOTICES.md` (D-N-8, D-N-16) on the GitHub release, with `SHA256SUMS`, an SPDX SBOM per binary, build provenance attestations and `install.sh` (D-N-7, D-N-15). |
| Docker image | `ghcr.io/<owner>/cetra:<version>`, `linux/amd64` and `linux/arm64`. `gcr.io/distroless/static-debian13:nonroot` pinned by digest, uid/gid 65532, `/data` owned by it, `VOLUME /data`, `EXPOSE 8080 4317 4318 9464`, healthcheck `cetra health` (`/readyz`). Built from the release archives (D-N-3, D-N-4). |
| docker-compose | `deploy/compose/compose.yaml` (`cetra` with a named volume) and the overlay `compose.s3.yaml` (a single-node Garage and its init, S3 mode). Compose settings in `.env`, server settings in `cetra.env` (D-N-9). |
| Helm chart | `deploy/helm/cetra`, pushed to `oci://ghcr.io/<owner>/charts/cetra`, chart version = app version. One `StatefulSet` (`replicas: 1`, not configurable), a `volumeClaimTemplates` volume for `/data` kept on delete, `Service` (8080, 4317, 4318), optional `Ingress`, config through `config` (rendered to `cetra.toml`), `env`, a Secret (`existingSecret` or chart-made) and `s3.*`. `podManagementPolicy: OrderedReady`, `terminationGracePeriodSeconds: 60`, a rolling update that replaces the one pod (never two pods on one volume) (D-N-10). |

Versions: SemVer, one version for the binary, the image and the chart; `CHANGELOG.md`; a tag `v<version>` on `main` runs the release workflow. Until the first public release, releases are draft pre-releases and the GHCR packages are private (D-N-5, D-N-6).

A file lock (`flock` on `<data_dir>/LOCK`) prevents two processes from opening the same data dir.

## Process lifecycle

- **Startup:** load config → check that `meta.db` is not of a newer format or schema (04 § Format versions; the preflight, before anything else of the data directory opens) (SF1) → lock data dir and open storage (`catalog.db`, replay WAL) → open `meta.db` + migrations → (log the setup URL if no user exists) → start background tasks (among them the deletion worker, every 2 s and on each request, and the hourly retention task, D-M-3, D-M-10) → bind ports → `/readyz` = 200 (D137). After `meta.db`: finish an interrupted key rotation, load or generate the encryption key, and verify its check value (D-I-1, D-I-3). Then `running` eval jobs go back to `queued` (a job interrupted a fourth time fails), `running` backfills are marked `failed` (`interrupted`, D-I-39), and the eval dispatcher, janitor, pruner, budget ticker, the trigger's flusher and sweeper and the backfill runner start, each running one pass at once (D-I-12). Pending traces are loaded from `pending_traces` before the ports open (D-I-8); when the rules cannot be read, their rows are held until the first sweep sets the tracked projects.
- **Shutdown (SIGTERM/SIGINT):** close live streams, stop accepting connections, drain in-flight requests (≤ 20 s), stop the usage loop (≤ 2 s; a flush in progress commits first), stop the eval loops (the backfill runner stops between pages; if a page read is still in flight when the eval loops' drain ends, the runner is aborted with them; either way the backfill stays `running` and the next start fails it as `interrupted`, D-I-39) and give eval workers 3 s, then abort them (their jobs stay `running` until the next start re-queues them, D-I-12), wait for detached OTLP commits (≤ 5 s, D-G-38) and flush the trigger's pending map in full, every take, so the traces those commits recorded are kept (D-I-8), flush `last_used` and usage counters, flush memtables (best effort ≤ 30 s), close SQLite. The bounded steps add up to 60 s, the whole grace period of the container and chart; the pending-map, `last_used` and usage flushes and the SQLite close are short meta writes outside that bound.
- `/healthz`: the process is alive, and no storage failure that only a restart clears is set (a WAL, a local object fsync or a catalog commit failed, D-R-6); otherwise 503 `busy` naming them, so a liveness probe restarts the server (D-N-35). `/readyz`: `meta.db` answers, neither WAL has failed, no local object fsync or catalog commit has failed (D-R-6), free disk is above `storage.min_free_bytes`, and the object store answers (a HEAD of the marker `_probe/readyz`, or a PUT of it when the HEAD fails; 2 s timeout; the result is reused for 10 s; D-N-36). A failure answers 503 `busy` naming the failed checks (D182, D-N-30). Liveness probes use `/healthz`: a full disk or an unreachable bucket takes the server out of rotation but must not restart it.

## Licence keys, plans and entitlements (track P)

The design is `docs/plans/p-entitlements/00-design.md` (D169–D179, D-P-1–D-P-25).

**Open core.** The core is MIT. `crates/server/src/ee/`, `crates/token/`, `web/src/ee/` and `tools/license/` are under the Elastic License 2.0 and compiled into the same binary (D172, D-P-1, D-P-2). Every source file there starts with `// SPDX-License-Identifier: Elastic-2.0`; `just check-ee` checks the headers and the licence files.

**Signed tokens.** Licence keys, org entitlement tokens and control-plane requests share one format, implemented once in the `cetra-token` crate (Elastic-2.0, D-P-26): `cetra1.<kid>.<base64url JSON payload>.<base64url ed25519 signature>`, the signature covering `cetra1.<kid>.<payload>`, at most 16 KiB. Release builds trust only the public keys compiled into the binary, each bound to the kinds it may sign: `license-offline-1`, `license-1` and `license-2` licences (`license-1` online on the control plane, the others offline, D-P-73); `cloud-1` (and its offline spare `cloud-2`) org and control-plane tokens. Debug builds also trust the development key `dev-1` for every kind (D-P-5, D-P-6, D-P-27).

**Licence key** (`CETRA_LICENSE_KEY`, self-hosted). Payload: `kind: "license"`, `id`, `issued_at`, `customer`, `features`, `expires_at`, `grace_ends_at`. Active before `expires_at`, in grace (features on, warnings) before `grace_ends_at`, expired after. A refused key is logged by reason and the server runs without a licence. Without a key: every core feature, no limits, enterprise features locked. Licences carry no limits in the MVP (D-P-7). When a licence lapses, restrictions already set keep applying (D-P-8).

**Entitlements.** One value per org, resolved in memory at every check: instance defaults (`[entitlements.defaults]`, from track P's M2), replaced by the org's valid token, plus the licence's features (D-P-10). Features: `retention_management`, `project_roles`, `sso_enforcement`, `org_sso`. Limits: `units_included`, `units_cap`, `ingest_spans_per_sec`, `ingest_burst`, `data_access_days`, `max_seats`, `max_projects`, `max_alert_rules`, `max_active_online_rules`, `max_annotation_queues`; none means unlimited (D-P-12, D-P-13). There is no org judge-spend limit.

**Org tokens (cloud).** Only on a cell with `entitlements.control_plane = true` (D-P-50). The control plane owns the plan catalog and per-org overrides and `PUT`s a signed `kind: "org"` token, whose `audience` is the cell's `public_url`, to `/api/internal/v1/orgs/{org_id}/entitlements` on the org's cell; the newest `issued_at` wins, an expired token falls back to the instance defaults, and every shrink of the data window is recorded so retention uses the largest window of the last 30 days (D-P-9, D-P-11, D-P-30). The control plane creates orgs on a cell with `POST /api/internal/v1/orgs` (D-P-29). Its bearer tokens (`kind: "control_plane"`) name the one method and path they are for (D-P-71).

**Enforcement.**
- Monthly units (spans + scores, judge-trace spans excluded) per UTC calendar month, from `usage_daily` plus unflushed counts. At `units_cap`, OTLP HTTP, score writes and feedback answer `402 quota_exceeded` (gRPC `RESOURCE_EXHAUSTED` without `RetryInfo`; score deletions are never refused); warnings at 80 % and 100 % of `units_included` (banner, one email to owners, D-P-38); the 402 carries `Retry-After` until the reset, and the SDKs pause for at most one hour at a time (D-P-16–D-P-18, D-P-34, D-P-37).
- Ingest rate: a per-org token bucket over accepted spans; `429 rate_limited` with `Retry-After`, gRPC `UNAVAILABLE` (D-P-19, D-P-35).
- Seats (owner, admin and member memberships plus pending invites with those roles) and active projects: `403 plan_limit` on invite, role change, accept and project create (D-P-20).
- Data older than `data_access_days` is hidden from every read by `QueryOptions::visible_from`, applied in the base query views (media blobs too); deletion reads are exempt; retention uses the largest window of the last 30 days plus 30 days, and the ingest age check the current window plus 30 days (D175, D-P-21, D-P-22, D-P-45, D-P-47).
- Feature gates answer `403 plan_required`; count limits (alert rules, active online rules) answer `403 plan_limit` (D-P-14, D-P-23).

**Metering.** `usage_daily` (spans, bytes, scores, judge tokens and cost) per org, project and day is the billing source. It is exposed on the org's Plan page (track P's M3), through `GET /api/v1/orgs/{org}/usage`, and to the control plane through `GET /api/internal/v1/usage` with a `kind: "control_plane"` bearer token (D-P-24).

## Self-monitoring

> **Built in N M3 (D-N-28 to D-N-30, D-N-33):**
> - Prometheus `/metrics` on `metrics_addr`, a separate listener, off by default, with no authentication. The metrics are listed in `website/src/content/docs/docs/self-hosting/monitoring.md`. Labels come from fixed sets only, never a tenant, user, key, model or URL (D-N-29).
> - `/readyz` as § Process lifecycle describes it.
> - Restore mode (`restore_mode`, D-N-25) for restore drills: no outbound work.
>
> OTel self-traces stay deferred (D38).

## Backups (documented procedure, D37)

What to back up: `meta.db` and `catalog.db` (taken together, in the same pause), the object store (`objects/` or the bucket), and `<data_dir>/secret.key` (or keep `CETRA_ENCRYPTION_KEY`) with the data. Without it, LLM connection keys and webhook secrets cannot be read, and the server refuses to start (D-I-1). `cetra admin rotate-encryption-key` replaces the key while the server is stopped (D-I-3). A backup taken before a rotation can only be read with the key that was current then; keep that key with it.

**Cold snapshot (the MVP procedure, D-N-11; `website/src/content/docs/docs/self-hosting/backup.md`):** stop the server (it drains and flushes, at most 60 s); copy the whole data directory (`meta.db`, `catalog.db`, `wal/`, `secret.key`, and `objects/` in local mode; `tmp/` and `LOCK` may be skipped); in S3 mode, copy the bucket prefix during the same stop; start the server.

**Restore:** stop the server, keep the current data directory (or a copy of its `meta.db`), put the backed-up data directory in place, in S3 mode make the bucket prefix identical to the backup copy (sync with delete). Then, with the server still stopped, run `cetra admin migrate` if the backup is from an older version, and re-apply deletions (below) before starting the server.

**GDPR and backups:** deleted data persists in backups until they expire; the docs recommend a backup retention ≤ 30 days. Before a restore, keep the current data directory (or a copy of its `meta.db`); after it, with the server stopped, run `cetra admin migrate` if the backup is from an older version, then `cetra admin reapply-deletions --from <kept data_dir>`: it copies every deletion the restored `meta.db` lacks, with its scope, and the server redoes them within seconds of starting, before it serves requests (D-M-9). Evaluation jobs queued in the restored data are not cancelled by this step. A rollback (the old binary on the restored pre-upgrade copy) works the same way: the kept directory is at a newer schema, and the command reads it by column name. Pre-migration copies (`meta.db.pre-m<N>`, `catalog.db.pre-m<N>`) are backups too: they keep erased data, count toward the ≤ 30-day backup retention, and should be deleted once the upgrade is verified.

**Online backups (track N's M2, D-N-17–D-N-24):** `cetra admin backup --to <dir> [--meta-only]` asks the running server (it holds the data directory's lock) through the control socket `<data_dir>/admin.sock`. The server flushes both tables, copies `meta.db` and then `catalog.db` with `VACUUM INTO` (the catalog copy under the committer lock, pinning every object it lists), hard-links or copies those objects in local mode, copies `secret.key`, and writes `manifest.json` last. The socket speaks one JSON line each way, each carrying the protocol version `"v": 1`; a request of another version is answered `unsupported_protocol`, and a client refuses an answer of another version; within version 1 fields, commands and error codes are only added (D-R-36) (SF4). `manifest.json` is `format: 1`, whose keys are only added; it records the formats and last migrations of both databases (`meta_format`, `meta_schema`, `storage_format`, `catalog_schema`), read from the copies (D-R-37) (SF5). Nothing reads the manifest. A backup from a newer release is refused at restore: `gc-orphans` and `migrate` check `meta.db` first and stop before changing anything (04 § Format versions) (SF5). Compaction, deletion and retention keep running. In S3 mode the backup is metadata-only and stays restorable against the live bucket for `storage.gc.grace`, which may be up to 30 days (no separate `gc.object_retention` key). A restore puts the files back with the server stopped, then runs `cetra admin gc-orphans` (deletes objects the restored catalog does not know, reports needed objects the store lacks), then `cetra admin migrate` if the backup is from an older version, then `reapply-deletions`, and only then starts the server.

## Upgrades

- Semantic versioning (0.x for now). Upgrade: take a backup, replace the binary, image or chart version, restart; SQLite migrations run automatically (forward-only). Release notes (`CHANGELOG.md`) start with `### Upgrade notes` when an upgrade needs action (D-N-5, D-N-12; `website/src/content/docs/docs/self-hosting/upgrade.md`). Before migrating an existing `meta.db` or `catalog.db`, the server keeps a copy named `meta.db.pre-m<N>` / `catalog.db.pre-m<N>` (by `VACUUM INTO`), where N is the highest migration applied before the upgrade, and only the copy from the last upgrade (D-N-22). The copies hold the data: delete them once the upgrade is verified.
- Parquet schema changes are additive only (03 §3); normalization changes bump `schema_version` without rewriting data.
- From the first public release tag (D185): a storage format change bumps a format version and migrates at open (D-R-12); the WAL carries a format header in each segment (03 § 12), and a new binary reads the previous format's segments (replays them before writing the new format). Before that tag there is no format constraint.
- `meta.db`, `secret.key`, the backup manifest and the admin socket follow 04 § Format versions (D-R-33 to D-R-38): a `meta.db` format version beside the migrations, additive migrations, golden backups restored by CI (SF7).
- Downgrades are not supported: a directory written by a newer format is refused by an older binary before anything is changed (03 § 12); restore the pre-upgrade backup.

## Demo org (D187)

The website's "Try demo" link opens `https://<cell>/demo` on the cloud cell that holds the demo org (today `eu1`). Setting it up (the cetra team, once per cell; `tools/demo/README.md` has the steps):

1. Create the org through the control plane with a team account, so Q knows it like any org, and give it an override without a data-access window, units cap or ingest rate below the generator's (Q's request R-demo, see the plan).
2. Set `demo.org` to its id in the cell's config and restart the cell.
3. In the org, create the project, an `sk` key with `ingest, read`, the LLM connection to the generator's judge endpoint, the evaluators, online rules, score definitions, alert rules and dashboards the README lists.
4. Run the `cetra-demo` generator next to the cell (a third container in the cell's compose stack) with that key; its judge endpoint is served on a public hostname through the cell's reverse proxy, so `security.allow_private_network_egress` stays `false`.

The team's accounts are the demo org's only members, and every visitor sees their names and emails in the member list: use team addresses, not personal ones.

## Security checklist (MVP)

- All secrets encrypted at rest (04 §Encryption); keys/tokens hashed.
- Outbound requests (webhooks, LLM connections, OIDC, Slack) go through an HTTP client with an SSRF guard. Unless `security.allow_private_network_egress = true` (needed for local Ollama/vLLM — the LLM connection form explains the setting; the UI cannot read its value (D-I-4)), the client drops every resolved address, and any literal IP in the URL, in these ranges (D166): loopback, RFC 1918, CGNAT `100.64.0.0/10`, link-local, unique local `fc00::/7`, unspecified, multicast, broadcast, `192.0.0.0/24`, `198.18.0.0/15`, documentation ranges, and IPv4-mapped or NAT64 forms of any of these. If none is left, the call fails before connecting. Redirects are never followed. `http` and `https` only. Identity providers from `[auth]` are operator configuration and are not passed through that guard; an org's own identity provider (track P's M5) is: every request (discovery, JWKS, token) is refused before sending when its URL is not `http(s)` or is a denied literal address, host names go through the guarded resolver, and bodies are capped at 1 MiB (D-P-53). The OIDC client refuses redirects and times out after 10 seconds.
- Request size limits everywhere; JSON depth limit 128.
- Credentials are never taken from query strings. Request logs carry the matched route template, not the raw URI (D150).
- Content-Security-Policy on the UI: `default-src 'self'; img-src 'self' blob: data:; media-src 'self'; style-src 'self' 'unsafe-inline'; base-uri 'none'; form-action 'self' http://127.0.0.1:*; frame-ancestors 'none'` (the form action allows the CLI sign-in callback on loopback, D-L-9; the port is dynamic), with `X-Content-Type-Options: nosniff` and `Referrer-Policy: same-origin`; payloads rendered as text (no HTML injection from trace content).
- `cargo deny` (advisories, bans, sources) on every CI run and `cargo deny check advisories` weekly (RustSec, the database `cargo audit` uses); Dependabot alerts for JS and Python; `cargo deny` checks dependency licences against a permissive allow-list (D-P-3).

## Documentation deliverables (part of the MVP)

Self-hosting (`website/src/content/docs/docs/self-hosting/`, served at cetra.sh/docs/self-hosting/: install by binary, Docker, compose or Helm, configuration and its reference, S3, backup/restore, upgrades; tracks N and W), quickstart (Docker, binary), OTel recipe per language, SDK guides (Python/TS), Vercel AI SDK guide, attribute reference, filter DSL + SQL schema reference, evaluators & online rules, alerts, auth/SSO setup (GitHub, Google, Okta, Entra ID, Keycloak), SMTP setup, S3 setup, backup/restore, GDPR guide, Helm values reference, API reference (rendered OpenAPI).
The site and its drift checks are track W (`docs/plans/docs-site/`). The configuration reference page is checked against the loader by `config::tests::the_configuration_reference_matches_the_loader`.
