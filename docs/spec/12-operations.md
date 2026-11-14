# 12 — Operations: configuration, packaging, limits, metering, backups, upgrades

## Configuration

Sources, later overriding earlier: built-in defaults → `llmobs.toml` (`--config` or `LLMOBS_CONFIG`) → env vars (`LLMOBS_` + upper-snake path, e.g. `LLMOBS_STORAGE__OBJECT_STORE` for `storage.object_store`; common ones also have short aliases listed below). Parsed with `figment`; unknown keys are an error. The log filter always caps the `rmcp` target at `warn`, since that crate logs MCP arguments and results at `debug` (D-H-11). `LLMOBS_URL`, `LLMOBS_TOKEN` and `LLMOBS_PROJECT` belong to the CLI's remote commands (D-H-12) and are not config keys. `llmobs admin check` validates config, data dir, encryption key, SMTP and object store connectivity.

GitHub and Google also accept the split form (`LLMOBS_AUTH__GITHUB__CLIENT_ID`, `LLMOBS_AUTH__GOOGLE__CLIENT_SECRET`, and the other scalar fields of those tables). `[[auth.oidc]]` is TOML only; `LLMOBS_AUTH__OIDC__*` is ignored.

| Key | Env alias | Default |
|---|---|---|
| `data_dir` | `LLMOBS_DATA_DIR` | `./llmobs-data` |
| `public_url` | `LLMOBS_PUBLIC_URL` | `http://localhost:8080` |
| `http_addr` | `LLMOBS_HTTP_ADDR` | `0.0.0.0:8080` |
| `trusted_proxies` | `LLMOBS_TRUSTED_PROXIES` | empty (comma-separated CIDRs, D142) |
| `grpc_addr` | `LLMOBS_GRPC_ADDR` | `0.0.0.0:4317` (empty = disabled; a config key from track G M3) |
| `otlp_http_addr` | `LLMOBS_OTLP_HTTP_ADDR` | `0.0.0.0:4318` (empty = disabled) |
| `storage.object_store` | `LLMOBS_OBJECT_STORE` | `file://<data_dir>/objects`. A set value is `file://<absolute path>` or `s3://<bucket>[/<prefix>]` |
| `storage.s3.region` | `AWS_REGION` | unset |
| `storage.s3.endpoint` | | unset (MinIO, R2, …) |
| `storage.s3.force_path_style` | | `false` |
| `storage.s3.allow_http` | | `false` |
| `encryption_key` | `LLMOBS_ENCRYPTION_KEY` | unset: read `<data_dir>/secret.key`, generated on the first start of a new data directory (D-I-1) |
| `license_key` | `LLMOBS_LICENSE_KEY` | unset: no licence, enterprise features locked (D172, D-P-7) |
| `entitlements.defaults.plan_name` | | `"Self-hosted"`: the plan name shown for orgs without a token (D-P-10) |
| `entitlements.defaults.features` | | unset: every core feature (`retention_management`); a list grants only those; `ee` names are refused (D-P-10) |
| `entitlements.defaults.<limit>` | | unset = unlimited, for each of `units_included`, `units_cap`, `ingest_spans_per_sec`, `ingest_burst`, `data_access_days`, `max_seats`, `max_projects`, `max_alert_rules`, `max_active_online_rules` (D-P-13) |
| `orgs.user_create` | | `true`; `false` on a cloud cell: users cannot create orgs, the control plane does (D-P-31) |
| `orgs.create_url` | | unset: where "New organization" links when `orgs.user_create` is false (D-P-31) |
| `entitlements.upgrade_url` | | unset: no Upgrade button. `{org}` is replaced by the org id (D-P-15) |
| `entitlements.control_plane_provider` | | `"oidc:cloud"`: the identity provider whose subjects the control plane uses (D-P-39) |
| `smtp_url`, `email_from` | `LLMOBS_SMTP_URL`, `LLMOBS_EMAIL_FROM` | unset (email off) |
| `auth.*` | see 08 | |
| `init.admin_email` | `LLMOBS_INIT_ADMIN_EMAIL` | unset |
| `init.admin_password` | `LLMOBS_INIT_ADMIN_PASSWORD` | unset |
| `init.org` | `LLMOBS_INIT_ORG` | unset |
| `init.project` | `LLMOBS_INIT_PROJECT` | unset |
| `init.secret_key` | `LLMOBS_INIT_SECRET_KEY` | unset (optional `sk-llmobs-` key for headless init) |
| `ingest.max_request_bytes` | | `"10MiB"` (before and after decompression) |
| `ingest.max_spans_per_request` | | `50000` |
| `ingest.max_inflight_bytes` | | `"256MiB"` (request bytes held at once, D-G-5) |
| `ingest.max_grpc_message_bytes` | | `"32MiB"` (with the gRPC listener, track G M3) |
| `ingest.max_attribute_bytes` | | `"8MiB"` |
| `media.min_bytes` / `media.max_bytes` | | `"4KiB"` / `"32MiB"` |
| `storage.wal.sync` | | `"always"` (or a duration such as `"5ms"`: fsync at most that often) |
| `storage.flush.max_bytes` / `storage.flush.max_age` / `storage.flush.upload_concurrency` | | `"256MiB"` / `"120s"` / `8` |
| `storage.memtable.hard_limit` | | `"1GiB"` |
| `storage.compaction.target_file_size` / `storage.compaction.threads` | | `"512MiB"` / `max(1, cores/4)` |
| `storage.compaction.interval` / `storage.compaction.close_delay` | | `"30s"` / `"2h"` |
| `storage.compaction.size_ratio` / `storage.compaction.max_memory` | | `4` / `"1GiB"` |
| `storage.compaction.l0_files` / `storage.compaction.l0_bytes` / `storage.compaction.l0_age` | | `8` / `"512MiB"` / `"10m"` |
| `storage.gc.grace` | | `"5m"` |
| `storage.query.max_memory` | | `"2GiB"` |
| `storage.query.metadata_cache_bytes` | | `"512MiB"` |
| `storage.deletion.max_delay` | | `"24h"` |

The storage engine's settings form the `[storage]` section, with one sub-table per area (`wal`, `s3`, `flush`, `memtable`, `compaction`, `query`, `gc`, `deletion`, `anchor`); spec 03 names them without the `storage.` prefix. Sizes are strings with a binary unit (`"256MiB"`, `"1GiB"`) and durations are strings with a unit (`"2ms"`, `"120s"`, `"24h"`) (D90). The `[ingest]` and `[media]` sections use the same formats. S3 credentials are the standard `AWS_*` environment variables and are not config keys. The full list, with defaults, is the `StorageConfig` doc in `llmobs-storage`.
| `eval.workers` | | `16` (1–256, D-I-6) |
| `security.allow_private_network_egress` | | `false` (D166) |
| `log.format` / `log.level` | `LLMOBS_LOG` | `pretty` on TTY else `json` / `info` |
| `telemetry.enabled` | `LLMOBS_TELEMETRY` | decided with the license (anonymous usage ping, opt-out) |

TLS: terminated by a reverse proxy in the reference setups; native TLS optional via `tls.cert` / `tls.key` (rustls) for simple installs.

## Packaging

Track N builds, publishes and tests these (`docs/plans/n-packaging/`, D-N-1–D-N-16). User documentation: `docs/self-hosting/`.

| Artifact | Details |
|---|---|
| Binary | `llmobs` for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (static) and `aarch64-apple-darwin` (D-N-1). Web UI embedded. Built natively per platform with `cargo auditable build --release --locked` (the workspace release profile: LTO thin, `codegen-units=1`, unwinding kept for task isolation). Shipped as `llmobs-<version>-<target>.tar.gz` with `LICENSE`, `LICENSE-ELASTIC` and `THIRD-PARTY-NOTICES.md` (D-N-8, D-N-16) on the GitHub release, with `SHA256SUMS`, an SPDX SBOM per binary, build provenance attestations and `install.sh` (D-N-7, D-N-15). |
| Docker image | `ghcr.io/<owner>/llmobs:<version>`, `linux/amd64` and `linux/arm64`. `gcr.io/distroless/static-debian13:nonroot` pinned by digest, uid/gid 65532, `/data` owned by it, `VOLUME /data`, `EXPOSE 8080 4317 4318`, healthcheck `llmobs health` (`/readyz`). Built from the release archives (D-N-3, D-N-4). |
| docker-compose | `deploy/compose/compose.yaml` (`llmobs` with a named volume) and the overlay `compose.s3.yaml` (a single-node Garage and its init, S3 mode). Compose settings in `.env`, server settings in `llmobs.env` (D-N-9). |
| Helm chart | `deploy/helm/llmobs`, pushed to `oci://ghcr.io/<owner>/charts/llmobs`, chart version = app version. One `StatefulSet` (`replicas: 1`, not configurable), a `volumeClaimTemplates` volume for `/data` kept on delete, `Service` (8080, 4317, 4318), optional `Ingress`, config through `config` (rendered to `llmobs.toml`), `env`, a Secret (`existingSecret` or chart-made) and `s3.*`. `podManagementPolicy: OrderedReady`, `terminationGracePeriodSeconds: 60`, a rolling update that replaces the one pod (never two pods on one volume) (D-N-10). |

Versions: SemVer, one version for the binary, the image and the chart; `CHANGELOG.md`; a tag `v<version>` on `main` runs the release workflow. Until the product name is chosen (D46), releases are draft pre-releases and the GHCR packages are private (D-N-5, D-N-6).

A file lock (`flock` on `<data_dir>/LOCK`) prevents two processes from opening the same data dir.

## Process lifecycle

- **Startup:** load config → lock data dir and open storage (`catalog.db`, replay WAL) → open `meta.db` + migrations → (log the setup URL if no user exists) → start background tasks (among them the deletion worker, every 2 s and on each request, and the hourly retention task, D-M-3, D-M-10) → bind ports → `/readyz` = 200 (D137). After `meta.db`: finish an interrupted key rotation, load or generate the encryption key, and verify its check value (D-I-1, D-I-3). Then `running` eval jobs go back to `queued` (a job interrupted a fourth time fails), and the eval dispatcher, janitor, pruner and budget ticker start, each running one pass at once (D-I-12).
- **Shutdown (SIGTERM/SIGINT):** stop accepting connections, drain in-flight requests (≤ 20 s), stop the eval loops and give eval workers 3 s, then abort them (their jobs stay `running` until the next start re-queues them, D-I-12), flush memtables (best effort ≤ 30 s), close SQLite.
- `/healthz`: process alive. `/readyz`: WAL writable, SQLite reachable, object store reachable (cached 10 s), free disk above threshold.

## Licence keys, plans and entitlements (track P)

The design is `docs/plans/p-entitlements/00-design.md` (D169–D179, D-P-1–D-P-25).

**Open core.** The core is MIT. `crates/server/src/ee/`, `crates/token/`, `web/src/ee/` and `tools/license/` are under the Elastic License 2.0 and compiled into the same binary (D172, D-P-1, D-P-2). Every source file there starts with `// SPDX-License-Identifier: Elastic-2.0`; `just check-ee` checks the headers and the licence files.

**Signed tokens.** Licence keys, org entitlement tokens and control-plane requests share one format, implemented once in the `llmobs-token` crate (Elastic-2.0, D-P-26): `llmobs1.<kid>.<base64url JSON payload>.<base64url ed25519 signature>`, the signature covering `llmobs1.<kid>.<payload>`, at most 16 KiB. Release builds trust only the public keys compiled into the binary, each bound to the kinds it may sign: `prod-1` licences; `cloud-1` (and its offline spare `cloud-2`) org and control-plane tokens. Debug builds also trust the development key `dev-1` for every kind (D-P-5, D-P-6, D-P-27).

**Licence key** (`LLMOBS_LICENSE_KEY`, self-hosted). Payload: `kind: "license"`, `id`, `issued_at`, `customer`, `features`, `expires_at`, `grace_ends_at`. Active before `expires_at`, in grace (features on, warnings) before `grace_ends_at`, expired after. A refused key is logged by reason and the server runs without a licence. Without a key: every core feature, no limits, enterprise features locked. Licences carry no limits in the MVP (D-P-7). When a licence lapses, restrictions already set keep applying (D-P-8).

**Entitlements.** One value per org, resolved in memory at every check: instance defaults (`[entitlements.defaults]`, from track P's M2), replaced by the org's valid token, plus the licence's features (D-P-10). Features: `retention_management`, `project_roles`, `sso_enforcement`, `org_sso`. Limits: `units_included`, `units_cap`, `ingest_spans_per_sec`, `ingest_burst`, `data_access_days`, `max_seats`, `max_projects`, `max_alert_rules`, `max_active_online_rules`; none means unlimited (D-P-12, D-P-13). There is no org judge-spend limit.

**Org tokens (cloud).** The control plane owns the plan catalog and per-org overrides and `PUT`s a signed `kind: "org"` token, whose `audience` is the cell's `public_url`, to `/api/internal/v1/orgs/{org_id}/entitlements` on the org's cell; the newest `issued_at` wins, an expired token falls back to the instance defaults, and every shrink of the data window is recorded so retention uses the largest window of the last 30 days (D-P-9, D-P-11, D-P-30). The control plane creates orgs on a cell with `POST /api/internal/v1/orgs` (D-P-29).

**Enforcement.**
- Monthly units (spans + scores, judge-trace spans excluded) per UTC calendar month, from `usage_daily` plus unflushed counts. At `units_cap`, OTLP HTTP, score writes and feedback answer `402 quota_exceeded` (gRPC `RESOURCE_EXHAUSTED` without `RetryInfo`; score deletions are never refused); warnings at 80 % and 100 % of `units_included` (banner, one email to owners, D-P-38); the 402 carries `Retry-After` until the reset, and the SDKs pause for at most one hour at a time (D-P-16–D-P-18, D-P-34, D-P-37).
- Ingest rate: a per-org token bucket over accepted spans; `429 rate_limited` with `Retry-After`, gRPC `UNAVAILABLE` (D-P-19, D-P-35).
- Seats (owner, admin and member memberships plus pending invites with those roles) and active projects: `403 plan_limit` on invite, role change, accept and project create (D-P-20).
- Data older than `data_access_days` is hidden from every read by `QueryOptions::visible_from` in the storage query (media blobs too); deletion scope reads are exempt; retention and the ingest age check use the window plus 30 days (D175, D-P-21, D-P-22).
- Feature gates answer `403 plan_required`; count limits (alert rules, active online rules) answer `403 plan_limit` (D-P-14, D-P-23).

**Metering.** `usage_daily` (spans, bytes, scores, judge tokens and cost) per org, project and day is the billing source. It is exposed on the org's Plan page (track P's M3), through `GET /api/v1/orgs/{org}/usage`, and to the control plane through `GET /api/internal/v1/usage` with a `kind: "control_plane"` bearer token (D-P-24).

## Self-monitoring

Deferred to just before release (D38). Placeholder decisions: Prometheus `/metrics` on a separate admin port, OTel self-traces opt-in.

## Backups (documented procedure, D37)

What to back up: `meta.db` and `catalog.db` (taken together, in the same pause), the object store (`objects/` or the bucket), and `<data_dir>/secret.key` (or keep `LLMOBS_ENCRYPTION_KEY`) with the data. Without it, LLM connection keys and webhook secrets cannot be read, and the server refuses to start (D-I-1). `llmobs admin rotate-encryption-key` replaces the key while the server is stopped (D-I-3). A backup taken before a rotation can only be read with the key that was current then; keep that key with it.

**Cold snapshot (the MVP procedure, D-N-11; `docs/self-hosting/backup.md`):** stop the server (it drains and flushes, at most 60 s); copy the whole data directory (`meta.db`, `catalog.db`, `wal/`, `secret.key`, and `objects/` in local mode; `tmp/` and `LOCK` may be skipped); in S3 mode, copy the bucket prefix during the same stop; start the server.

**Restore:** stop the server, keep the current data directory (or a copy of its `meta.db`), put the backed-up data directory in place, in S3 mode make the bucket prefix identical to the backup copy (sync with delete), start. Then re-apply deletions (below).

**GDPR and backups:** deleted data persists in backups until they expire; the docs recommend a backup retention ≤ 30 days. Before a restore, keep the current data directory (or a copy of its `meta.db`); after it, start `llmobs serve` once if the backup is from an older version, then run `llmobs admin reapply-deletions --from <kept data_dir>`: it copies every deletion the restored `meta.db` lacks, with its scope, and the server redoes them (D-M-9).

**Online backups (track N's M2, planned in `docs/plans/n-packaging/m2/`, D-N-17–D-N-24):** `llmobs admin backup --to <dir> [--meta-only]` asks the running server (it holds the data directory's lock) through the control socket `<data_dir>/admin.sock`. The server flushes both tables, copies `meta.db` and then `catalog.db` with `VACUUM INTO` (the catalog copy under the committer lock, pinning every object it lists), hard-links or copies those objects in local mode, copies `secret.key`, and writes `manifest.json` last. Compaction, deletion and retention keep running. In S3 mode the backup is metadata-only and stays restorable against the live bucket for `storage.gc.grace`, which may be up to 30 days (no separate `gc.object_retention` key). A restore puts the files back with the server stopped, then runs `llmobs admin gc-orphans` (deletes objects the restored catalog does not know, reports needed objects the store lacks) before starting and `reapply-deletions`.

## Upgrades

- Semantic versioning (0.x for now). Upgrade: take a backup, replace the binary, image or chart version, restart; SQLite migrations run automatically (forward-only). Release notes (`CHANGELOG.md`) start with `### Upgrade notes` when an upgrade needs action (D-N-5, D-N-12; `docs/self-hosting/upgrade.md`). Automatic pre-migration copies `meta.db.pre-<version>` and `catalog.db.pre-<version>` are track N's M2 (not built).
- Parquet schema changes are additive only (03 §3); normalization changes bump `schema_version` without rewriting data.
- WAL format carries a version byte in each segment header; a new binary reads the previous WAL version (replays it before writing the new format).
- Downgrades are not supported (documented: restore the pre-upgrade backup).

## Security checklist (MVP)

- All secrets encrypted at rest (04 §Encryption); keys/tokens hashed.
- Outbound requests (webhooks, LLM connections, OIDC, Slack) go through an HTTP client with an SSRF guard. Unless `security.allow_private_network_egress = true` (needed for local Ollama/vLLM — the LLM connection form explains the setting; the UI cannot read its value (D-I-4)), the client drops every resolved address, and any literal IP in the URL, in these ranges (D166): loopback, RFC 1918, CGNAT `100.64.0.0/10`, link-local, unique local `fc00::/7`, unspecified, multicast, broadcast, `192.0.0.0/24`, `198.18.0.0/15`, documentation ranges, and IPv4-mapped or NAT64 forms of any of these. If none is left, the call fails before connecting. Redirects are never followed. `http` and `https` only. Identity-provider URLs are operator configuration and are not passed through that guard. The OIDC client refuses redirects and times out after 10 seconds.
- Request size limits everywhere; JSON depth limit 128.
- Credentials are never taken from query strings. Request logs carry the matched route template, not the raw URI (D150).
- Content-Security-Policy on the UI: `default-src 'self'; img-src 'self' blob: data:; media-src 'self'; style-src 'self' 'unsafe-inline'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'`, with `X-Content-Type-Options: nosniff` and `Referrer-Policy: same-origin`; payloads rendered as text (no HTML injection from trace content).
- `cargo deny` (advisories, bans, sources) on every CI run and `cargo deny check advisories` weekly (RustSec, the database `cargo audit` uses); Dependabot alerts for JS and Python; `cargo deny` checks dependency licences against a permissive allow-list (D-P-3).

## Documentation deliverables (part of the MVP)

Self-hosting (`docs/self-hosting/`: install by binary, Docker, compose or Helm, configuration, S3, backup/restore, upgrades; track N), quickstart (Docker, binary), OTel recipe per language, SDK guides (Python/TS), Vercel AI SDK guide, attribute reference, filter DSL + SQL schema reference, evaluators & online rules, alerts, auth/SSO setup (GitHub, Google, Okta, Entra ID, Keycloak), SMTP setup, S3 setup, backup/restore, GDPR guide, Helm values reference, API reference (rendered OpenAPI).
