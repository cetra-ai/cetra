---
title: "Configuration reference"
description: "Every cetra setting: its key, environment variable, type, default and meaning."
---

This page lists every setting `cetra serve` reads, with its default.

cetra reads, in order, each overriding the previous: built-in defaults, a TOML file (`cetra serve --config <file>` or `CETRA_CONFIG=<file>`), then environment variables. Every key can be set as `CETRA_` followed by its path in upper case with `__` between levels: `storage.flush.max_age` is `CETRA_STORAGE__FLUSH__MAX_AGE`. The Environment column lists only the short names that exist besides this form. Unknown keys are an error, and so is an unknown `CETRA_` variable. Sizes are strings with a binary unit (`"256MiB"`) and durations are strings with a unit (`"120s"`, `"5ms"`, `"24h"`). A list can be a TOML array or, in an environment variable, a comma-separated string. `CETRA_URL`, `CETRA_TOKEN` and `CETRA_PROJECT` belong to the CLI's remote commands and are not settings.

For how to choose values, see [Configuration](/docs/self-hosting/configuration/). An array of tables, such as `[[auth.oidc]]`, is written `auth.oidc[].field` below.

## Top level

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `data_dir` | `CETRA_DATA_DIR` | path | `./cetra-data` | The directory that holds the WAL, `catalog.db`, `meta.db` and `secret.key`. `/data` in the Docker image. One process owns it. |
| `public_url` | `CETRA_PUBLIC_URL` | URL | `http://localhost:8080` | The `http` or `https` origin users open. Used in links, emails, cookies and OAuth redirects. No path, query or user info. |
| `http_addr` | `CETRA_HTTP_ADDR` | address | `0.0.0.0:8080` | Listen address of the web UI, the API, OTLP/HTTP and MCP. |
| `otlp_http_addr` | `CETRA_OTLP_HTTP_ADDR` | address | `0.0.0.0:4318` | Listen address of the OTLP/HTTP-only listener. An empty string disables it. Must differ from the other listeners. |
| `grpc_addr` | `CETRA_GRPC_ADDR` | address | `0.0.0.0:4317` | Listen address of OTLP/gRPC. An empty string disables it. |
| `metrics_addr` | `CETRA_METRICS_ADDR` | address | unset (off) | Listen address of the Prometheus `/metrics` endpoint. It has no authentication: bind it to a private address. |
| `trusted_proxies` | `CETRA_TRUSTED_PROXIES` | list of CIDRs | empty | Proxies whose `X-Forwarded-For` header cetra trusts. |
| `restore_mode` | | boolean | `false` | Serve reads and run no outbound work (alert deliveries, webhooks, judges, email, identity providers). For restore drills. The self-monitoring export still runs: it goes to your own endpoint. |
| `encryption_key` | `CETRA_ENCRYPTION_KEY` | secret | unset | 32 bytes in standard base64 that encrypt stored secrets. Unset: cetra reads `<data_dir>/secret.key` and generates it on the first start of a new data directory. |
| `license_key` | `CETRA_LICENSE_KEY` | secret | unset | The licence key that unlocks enterprise features. Unset: an installed key applies, else they stay locked. When set, it wins over a key installed from the admin settings. |
| `smtp_url` | `CETRA_SMTP_URL` | secret URL | unset (email off) | `smtp://` (STARTTLS required), `smtps://` (TLS from the first byte) or `smtp://host?tls=none` (a local relay), with optional `user:password@`. Needs `email_from`. |
| `email_from` | `CETRA_EMAIL_FROM` | mailbox | unset | The `From` address of emails, such as `cetra <noreply@example.com>`. Required when `smtp_url` is set. |

## `[storage]`

Object storage credentials are not settings: cetra reads the standard `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` variables. `storage.data_dir` is not a key: set `data_dir` at the top level.

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `storage.min_free_bytes` | | size | `"2GiB"` | Ingestion is refused while free disk space is below this. It should cover at least one second of the peak write rate. |
| `storage.object_store` | `CETRA_OBJECT_STORE` | URL | `file://<data_dir>/objects` | Where Parquet files and media live: `file://<absolute path>` or `s3://<bucket>[/<prefix>]`. See [Object storage](/docs/self-hosting/s3/). |
| `storage.s3.region` | | string | unset | The bucket's region. Unset: the `AWS_REGION` environment variable. |
| `storage.s3.endpoint` | | URL | unset (AWS) | The endpoint of an S3-compatible provider such as MinIO, R2 or Garage. |
| `storage.s3.force_path_style` | | boolean | `false` | Address the bucket as `endpoint/bucket` instead of a subdomain. |
| `storage.s3.allow_http` | | boolean | `false` | Allow an `http://` endpoint. Only for a private network. |
| `storage.wal.sync` | | string | `"always"` | `"always"` syncs every group of writes before acknowledging. A duration such as `"5ms"` syncs at most that often: a power loss can drop the last interval, a process crash cannot. |
| `storage.wal.segment_bytes` | | size | `"64MiB"` | Start a new WAL segment after this many bytes. At least `"1MiB"`. |
| `storage.wal.group_max_bytes` | | size | `"8MiB"` | Close a group of writes once it reaches this size. At least `"64KiB"`. |
| `storage.wal.group_max_wait` | | duration | `"2ms"` | Close a group of writes this long after its first request. At most `"1s"`. |
| `storage.wal.skip_corrupt` | | boolean | `false` | Keep starting when a WAL record that is not the last one is corrupt. The corrupt record is lost. |
| `storage.flush.max_bytes` | | size | `"256MiB"` | Write the in-memory buffer to Parquet once it reaches this size. At least `"1MiB"` and below `storage.memtable.hard_limit`. |
| `storage.flush.max_age` | | duration | `"120s"` | Write the in-memory buffer to Parquet once its oldest row is this old. Between `"100ms"` and `"1h"`. |
| `storage.flush.upload_concurrency` | | integer | `8` | Parquet files one flush uploads at a time, 1 to 64. |
| `storage.memtable.hard_limit` | | size | `"1GiB"` | Refuse writes while the in-memory buffer holds more than this. At least `"16MiB"`. |
| `storage.query.target_partitions` | | integer | the number of CPU cores | Parallel partitions of one query. At least 1. |
| `storage.query.metadata_cache_bytes` | | size | `"512MiB"` | Memory for cached Parquet footers and page indexes. `"0B"` is allowed. |
| `storage.query.trace_lookback` | | duration | `"1h"` | How far back trace lists look, and the default window of `storage.anchor.window`. Up to `"24h"`. |
| `storage.query.max_memory_total` | | size | half the machine's memory, at most `"8GiB"` | Memory shared by all queries. The machine's cgroup limit counts when it is lower. At least `storage.query.max_memory`. |
| `storage.query.max_memory` | | size | `"2GiB"` | The most memory one query may hold. At least `"1MiB"`. |
| `storage.query.initial_memory` | | size | `"64MiB"` | Memory a query takes from the shared pool before it starts. Between `"1MiB"` and `storage.query.max_memory`. |
| `storage.query.admission_wait` | | duration | `"5s"` | How long a query waits for its initial memory before it fails as busy. At most `"60s"`. |
| `storage.query.read_cache_bytes` | | size | 10% of the file system of `data_dir`, at most `"100GiB"` | Size of the read cache for object storage, under `<data_dir>/cache`. `"0B"` turns it off. Unused with a local object store. |
| `storage.anchor.window` | | duration | unset (`storage.query.trace_lookback`) | How close to midnight UTC a span must start to need an anchor entry. Up to `"24h"`. |
| `storage.gc.grace` | | duration | `"5m"` | How long obsolete files stay before they are deleted, 1 second to `"30d"`. With object storage, also how long a metadata-only backup stays restorable. |
| `storage.compaction.interval` | | duration | `"30s"` | How often a compaction pass runs. Between `"100ms"` and `"1h"`. |
| `storage.compaction.threads` | | integer | a quarter of the CPU cores, at least 1 | Compaction jobs that run at the same time, 1 to 64. |
| `storage.compaction.target_file_size` | | size | `"512MiB"` | Compaction output files are cut at this size. Between `"1MiB"` and `"16GiB"`. |
| `storage.compaction.size_ratio` | | integer | `4` | A merge takes in a larger file only if the output so far is at least this many times its size. 2 to 100. |
| `storage.compaction.l0_files` | | integer | `8` | Merge the newest files once there are this many, 2 to 1000. |
| `storage.compaction.l0_bytes` | | size | `"512MiB"` | Merge the newest files once they total this size. Between `"1MiB"` and `"16GiB"`. |
| `storage.compaction.l0_age` | | duration | `"10m"` | Merge the newest files once the newest is this old. Between `"1s"` and `"24h"`. |
| `storage.compaction.close_delay` | | duration | `"2h"` | A day is closed and compacted this long after it ended. Between `"1s"` and `"7days"`. |
| `storage.compaction.max_memory` | | size | `"1GiB"` | Memory shared by running merges. Between `"16MiB"` and `"1TiB"`. |
| `storage.deletion.max_delay` | | duration | `"24h"` | How long a requested deletion may wait before the files are rewritten. Between `"1h"` and `"30days"`. |

## `[ingest]`

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `ingest.max_request_bytes` | | size | `"10MiB"` | The largest request body, before and after decompression. Between `"1KiB"` and `"1GiB"`. |
| `ingest.max_spans_per_request` | | integer | `50000` | The most spans in one request after decoding. At least 1. |
| `ingest.max_attribute_bytes` | | size | `"8MiB"` | A single attribute value longer than this is truncated. Between `"1KiB"` and `"64MiB"`. |
| `ingest.max_inflight_bytes` | | size | `"256MiB"` | Decompressed bytes processed at once across all requests. At least `ingest.max_request_bytes` and below `"4GiB"`. |
| `ingest.max_grpc_message_bytes` | | size | `"32MiB"` | The largest decoded OTLP/gRPC message. Between `"1KiB"` and `"1GiB"`, and at most `ingest.max_inflight_bytes`. |

## `[media]`

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `media.min_bytes` | | size | `"4KiB"` | Blobs smaller than this stay inline in the span. At least 1 byte and at most `media.max_bytes`. |
| `media.max_bytes` | | size | `"32MiB"` | Blobs larger than this are dropped. At most `"1GiB"`. |

## `[eval]`

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `eval.workers` | | integer | `16` | Evaluation jobs this process runs at once, 1 to 256. |

## `[audit]`

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `audit.retention_days` | | integer | `365` | Days an audit record is kept, then deleted by an hourly task. `0` keeps records forever; otherwise 30 or more. Applies to the instance log and to every org without its own `audit_retention_days` limit, which replaces it for that org on cetra Cloud (`0` keeps that org's records forever). |

## `[issues]`

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `issues.workers` | | integer | `4` | Workers that summarize and embed issue candidates (1–64). They share each connection's limits with judges. |

## `[security]`

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `security.allow_private_network_egress` | | boolean | `false` | Let LLM judges and webhooks reach private, loopback and link-local addresses. Needed for a local Ollama or vLLM. |

## `[entitlements]`

These settings decide what an organization gets without a licence key or a plan from a control plane. Most self-hosted installs leave them alone.

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `entitlements.upgrade_url` | | URL | unset | Where Upgrade buttons lead. `{org}` is replaced by the organization's ID. Unset: no button. |
| `entitlements.control_plane_provider` | | string | `"oidc:cloud"` | The identity provider whose subjects a control plane uses to name users. |
| `entitlements.control_plane` | | boolean | `false` | Set on a cloud cell only. When `false`, the `/api/internal/v1/*` routes answer 404, stored organization tokens are ignored and only licence keys are trusted. |
| `entitlements.defaults.plan_name` | | string | `"Self-hosted"` | The plan name shown for an organization without a token. 1 to 64 characters. |
| `entitlements.defaults.features` | | list | unset (every core feature) | The core features granted. A list grants only those. Enterprise features are refused here. |
| `entitlements.defaults.units_included` | | integer | unset (unlimited) | Units (spans plus scores) per month included in the plan. |
| `entitlements.defaults.units_cap` | | integer | unset (unlimited) | Units per month after which ingestion is refused. |
| `entitlements.defaults.ingest_spans_per_sec` | | integer | unset (unlimited) | Spans per second one organization may send. |
| `entitlements.defaults.ingest_burst` | | integer | unset (unlimited) | Burst size of the span rate limit. |
| `entitlements.defaults.data_access_days` | | integer | unset (unlimited) | Days of data visible in reads. |
| `entitlements.defaults.max_seats` | | integer | unset (unlimited) | Members and pending invites per organization. |
| `entitlements.defaults.max_projects` | | integer | unset (unlimited) | Active projects per organization. |
| `entitlements.defaults.max_alert_rules` | | integer | unset (unlimited) | Alert rules per organization. |
| `entitlements.defaults.max_active_online_rules` | | integer | unset (unlimited) | Active online rules per organization. |
| `entitlements.defaults.max_active_issue_sources` | | integer | unset (unlimited) | Enabled issue sources of active projects per organization. |
| `entitlements.defaults.max_annotation_queues` | | integer | unset (unlimited) | Annotation queues per organization (D-AQ-10). |

## `[alerts]`

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `alerts.email_recipients` | | `"members"` or `"any"` | unset: `"members"` when `auth.signup` is `"open"`, otherwise `"any"` | Whom email alert channels may reach. `"members"`: only verified members of the channel's organization, for alerts and test sends alike. `"any"`: any address. |

## `[orgs]`

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `orgs.user_create` | | boolean | `true` | Members may create organizations. `false` on a cloud cell, where the control plane creates them. |
| `orgs.create_url` | | URL | unset | Where "New organization" leads when `orgs.user_create` is `false`. Unset: no link. |

## `[demo]`

A demo org is an organization every signed-in user can read as a viewer without being a member. The limits apply only to these visitors, not to the org's real members.

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `demo.org` | | string | unset | ID of the demo org. It must exist and be active when cetra starts. Unset: no demo org. |
| `demo.requests_per_minute` | | integer | `120` | Requests a minute per visitor; above it the answer is `429`. At least 1. |
| `demo.user_in_flight` | | integer | `4` | Requests in progress at once per visitor. At least 1, and at most `demo.org_in_flight`. |
| `demo.org_in_flight` | | integer | `16` | Requests in progress at once across all visitors. At least 1. |
| `demo.sql_timeout` | | duration | `"10s"` | Longest SQL console query for a visitor, from 1 to 60 seconds. A longer timeout is lowered. |
| `demo.sql_max_rows` | | integer | `1000` | Most rows a SQL console answer returns to a visitor, from 1 to 10,000. |

These keys also take the split form, such as `CETRA_DEMO__ORG`.

## `[auth]`

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `auth.signup` | `CETRA_AUTH_SIGNUP` | string | `"invite_only"` | Who may create an account after the first user exists: `open`, `invite_only` or `disabled`. |
| `auth.signup_domains` | `CETRA_AUTH_SIGNUP_DOMAINS` | list | empty | Email domains allowed to sign up. Empty: no restriction. |
| `auth.disable_password` | `CETRA_AUTH_DISABLE_PASSWORD` | boolean | `false` | Refuse password sign-in and sign-up. Needs a configured provider. |
| `auth.require_email_verification` | `CETRA_AUTH_REQUIRE_EMAIL_VERIFICATION` | boolean | `false` | New accounts must verify their email before signing in. Needs `smtp_url`. |
| `auth.enforce_sso_domains` | `CETRA_AUTH_ENFORCE_SSO_DOMAINS` | list | empty | Email domains that must use single sign-on. Needs a configured provider. |
| `auth.github.client_id` | | string | | OAuth client ID of the GitHub app. Setting any `auth.github` key enables GitHub sign-in. |
| `auth.github.client_secret` | | secret | | OAuth client secret of the GitHub app. |
| `auth.github.link_existing_by_email` | | boolean | `true` | Link a verified GitHub email to an existing account. |
| `auth.google.client_id` | | string | | OAuth client ID for Google sign-in. Setting any `auth.google` key enables it. |
| `auth.google.client_secret` | | secret | | OAuth client secret for Google sign-in. |
| `auth.google.allowed_domains` | | list | empty | Email domains allowed to sign in with Google, checked against the verified email and the `hd` claim. |
| `auth.google.link_existing_by_email` | | boolean | `true` | Link a verified Google email to an existing account. |
| `auth.oidc[].name` | | string | | The provider's ID, 1 to 32 characters of `a-z`, `0-9` and `-`, unique among providers. |
| `auth.oidc[].display_name` | | string | | The name on the sign-in button. |
| `auth.oidc[].issuer` | | URL | | The issuer URL: `https`, or `http` on a loopback host. Multi-tenant Microsoft issuers are refused. |
| `auth.oidc[].client_id` | | string | | OAuth client ID. |
| `auth.oidc[].client_secret` | | secret | | OAuth client secret. Leave it out of the file and set `CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET` instead, with the provider's `name` in upper case and `-` written `_`. |
| `auth.oidc[].scopes` | | list | `["openid", "email", "profile"]` | Requested scopes. Must include `openid`. |
| `auth.oidc[].allowed_domains` | | list | empty | Email domains allowed to sign in with this provider. |
| `auth.oidc[].auto_provision.org` | | string | | Slug of the organization a new user joins. It does not need to exist yet. |
| `auth.oidc[].auto_provision.role` | | string | | The role a new user gets: `admin`, `member` or `viewer`. Never `owner`. |
| `auth.oidc[].link_existing_by_email` | | boolean | `false` | Link a verified email to an existing account. |

The GitHub and Google tables also accept the split form, such as `CETRA_AUTH__GITHUB__CLIENT_ID` and `CETRA_AUTH__GOOGLE__CLIENT_SECRET`. Of the `[[auth.oidc]]` tables, only the client secret has an environment form.

## `[init]`

These settings create the first admin, organization and project on a database that has none. Set the first four together or not at all.

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `init.admin_email` | `CETRA_INIT_ADMIN_EMAIL` | string | unset | Email of the first admin. |
| `init.admin_password` | `CETRA_INIT_ADMIN_PASSWORD` | secret | unset | Password of the first admin. |
| `init.org` | `CETRA_INIT_ORG` | string | unset | Name of the first organization. |
| `init.project` | `CETRA_INIT_PROJECT` | string | unset | Name of the first project. |
| `init.secret_key` | `CETRA_INIT_SECRET_KEY` | secret | unset | The project's ingest key, `sk-cetra-` followed by 32 letters or digits, so you know it in advance. Optional. |
| `init.read_key` | `CETRA_INIT_READ_KEY` | secret | unset | A second key of the project with scope `read` only, in the same format and different from `init.secret_key`. It reads traces and cannot ingest or write. Optional. |

## `[log]`

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `log.format` | | string | `pretty` on a terminal, `json` otherwise | `pretty` or `json`. |
| `log.level` | `CETRA_LOG` | string | `"info"` | A log filter such as `info` or `cetra_server=debug,warn`. |

## `[self_monitoring]`

Export cetra's own logs and traces to an OTLP/HTTP endpoint you choose (Sentry, Grafana Cloud, an OpenTelemetry Collector). Off while `otlp_endpoint` is unset. See [Monitoring](/docs/self-hosting/monitoring/#export-logs-over-otlp).

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `self_monitoring.otlp_endpoint` | | URL | unset (off) | Base URL of the OTLP/HTTP endpoint, `http` or `https`, without user info, query or fragment. cetra posts protobuf, gzip-compressed, to the URL followed by `/v1/logs` (logs) and `/v1/traces` (spans); a URL that already ends in `/v1/logs` or `/v1/traces` is refused. |
| `self_monitoring.otlp_headers` | | secret | unset | Request headers, `name=value` pairs separated by commas, as in `OTEL_EXPORTER_OTLP_HEADERS`. Values may be percent-encoded (`Basic%20…`). At most 32 headers and 8192 bytes; `content-type`, `content-encoding`, `content-length`, `host` and `user-agent` are set by cetra. |
| `self_monitoring.logs` | | boolean | `true` | Export log records. |
| `self_monitoring.traces` | | boolean | `true` | Export spans: HTTP requests, OTLP ingest, queries and background work. See [Monitoring](/docs/self-hosting/monitoring/#export-traces-over-otlp). |
| `self_monitoring.level` | | string | `"warn"` | A log filter for the exported events, like `log.level`. `rmcp` is capped at `warn`. Spans record the events that pass it. |
| `self_monitoring.trace_sample_ratio` | | number | `0.1` | The probability that a trace is exported, from `0.0` to `1.0`. Each root span is kept with this probability and its child spans follow it. `0.0` exports no span. |
| `self_monitoring.resource` | | string | `""` | Extra resource attributes, `key=value` pairs separated by commas, as in `OTEL_RESOURCE_ATTRIBUTES`. Values may be percent-encoded (`%2C` for a comma). At most 32 attributes and 8192 bytes. They are added after cetra's own (`service.name`, `service.version`, `service.instance.id`, `host.arch`, `os.type`) and replace one of the same key. |

## `[telemetry]`

What cetra sends to us: an anonymous heartbeat once a day, and crash reports if you turn them on. [Telemetry](/docs/self-hosting/telemetry/) lists every field. `CETRA_TELEMETRY=off`, `DO_NOT_TRACK=1`, restore mode, CI environments and debug builds turn it off whatever these settings say. `CETRA_TELEMETRY` is not the environment form of a setting: its only accepted values are `off`, `false` and `0`, and any other value stops cetra at startup.

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `telemetry.heartbeat` | | boolean | `true` | Send the anonymous daily heartbeat. |
| `telemetry.crash_reports` | | boolean | `false` | Send crash reports: each panic and error event as a code location and function names, never a message or a value. `CETRA_TELEMETRY=off`, `DO_NOT_TRACK=1` and restore mode turn it off; CI environments and debug builds do not. See [Crash reports](/docs/self-hosting/telemetry/#crash-reports). |
| `telemetry.endpoint` | | URL | `"https://telemetry.cetra.sh"` | Where the heartbeat goes, as `<endpoint>/v1/heartbeat`. `https`, or `http` to a loopback IP address such as `127.0.0.1` (not the name `localhost`). No user info, query or fragment. For a relay of your own. |

## `[analytics]`

Per-user product events sent through the server to a PostHog project of yours. Off while unset; restore mode turns it off. cetra's own cloud sets it. A self-hosted install sends us only bucketed feature counts in the heartbeat: see [Telemetry](/docs/self-hosting/telemetry/#product-analytics).

| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `analytics.posthog_host` | | URL | unset (off) | Your PostHog ingestion host, such as `https://eu.i.posthog.com`. cetra posts batches to `<host>/batch/`. `https`, or plain `http` to a private IP address (loopback, RFC 1918, `100.64.0.0/10`, `fc00::/7`). No user info, query or fragment. Set together with `posthog_key`. |
| `analytics.posthog_key` | | secret | unset | The PostHog project API key (`phc_…`). Set together with `posthog_host`. |
