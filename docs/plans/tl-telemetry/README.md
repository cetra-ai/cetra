# Track TL — cetra's own telemetry and error reporting: design

Status: **approved by the user** (2026-11-23), with every open question answered as proposed (§ Answers). Decisions `D-TL-<n>` (`docs/mvp-decisions.md` § Track TL). "(user)" marks the user's answers of 2026-11-23. One Sonnet cold review done and fixed (`side-session-notes/…/tl-design-review.md`).

Today `cetra serve` sends nothing anywhere: it writes logs to stdout and serves Prometheus `/metrics` when `metrics_addr` is set (D-N-28). This track adds six things, each documented and switchable:

1. **Self-monitoring over OTLP.** The operator points `cetra serve` at an OTLP/HTTP endpoint of their choice (Sentry, Grafana, an OpenTelemetry Collector, a Datadog Agent, another cetra) and the server exports its own logs, errors and traces there. Off by default.
2. **Our cloud uses the same mechanism.** Cells set those keys to a destination we run. The deploy wiring, the control plane and the choice of destination are track Q's (§ Requests to track Q).
3. **Opt-in crash reports to us.** One switch, off by default, sends panics and ERROR events to `telemetry.cetra.sh`, with code-defined facts only (user).
4. **`cetra debug-bundle`.** One local JSON file with build info, the config with secrets removed, recent logs and storage stats, for a GitHub issue.
5. **An anonymous daily heartbeat** from self-hosted installs, on by default (user), off with `CETRA_TELEMETRY=off`, `DO_NOT_TRACK` or in CI.
6. **Product analytics (user).** One typed catalog of product events: on our cloud, per-user events go to PostHog through the server; everywhere, bucketed feature counts ride on the heartbeat.

Research: `side-session-notes/…/tl-competitors.md` (Langfuse, Laminar, Opik, MLflow, Helicone).

## Goals

- An operator gets the server's logs, errors and request/background traces into the tool they already run, by setting one endpoint (and usually one auth header).
- Nothing leaves the process to us without being listed field by field on a docs page, announced at startup, and switchable with one environment variable.
- What is sent to us is built from typed values whose content is fixed by our code: no message text, no field values, no names, no hosts, no licence data. Tests prove a planted secret, email or prompt never reaches a report.
- A user filing a bug can attach one reviewable file instead of answering "which version, which config, what do the logs say".
- Licence verification stays offline (D172): the heartbeat shares no code path, no data and no endpoint with licences.

## Non-goals

- **SDK telemetry.** The Python and TypeScript SDKs send nothing to us (task scope).
- **Storing logs in cetra.** cetra still drops OTLP logs (D-G-40); a cetra destination receives the traces only (§ Signals).
- **Metrics over OTLP.** Prometheus `/metrics` stays the metrics interface (D-N-28); an OpenTelemetry Collector can scrape it and forward.
- **A vendor SDK** (Sentry or other) in the binary (task scope: one mechanism).
- **Joining a caller's trace.** Incoming `traceparent` headers are ignored (D-TL-7).
- **A UI for telemetry settings.** Config and environment only.
- **Remote configuration** of telemetry (MLflow fetches a config file each start). The binary does what its config says.
- **Server-side symbolication** of crash backtraces (§ 3).
- **Session replay, autocapture and any analytics script in the browser** (§ 6).
- **Per-user analytics on self-hosted installs** sent to us: only anonymous bucketed feature counts (§ 6).
- **Our cloud's deploy wiring and the relay service** themselves: track Q.

## What competitors do (summary)

| | Default | Opt-out | `DO_NOT_TRACK` | CI off | Counts | Error reporting to the vendor |
|---|---|---|---|---|---|---|
| Langfuse | heartbeat on, 12 h | `TELEMETRY_ENABLED=false`, overridden by an EE licence | no | `CI` | exact, plus top-30 user email domains and the licence key | none from self-hosted |
| Laminar | heartbeat on, 6 h | `LAMINAR_TELEMETRY_DISABLED` | no | no | exact row counts | Sentry only if the operator gives a DSN |
| Opik | daily report on | `OPIK_USAGE_REPORT_ENABLED=false` | no | no | exact | Python SDK: Sentry **on**, hard-coded DSN, no scrubbing |
| MLflow | per-call events on | `MLFLOW_DISABLE_TELEMETRY` | **yes** | ~15 CI variables | per call | none |
| Helicone | none | — | — | — | — | Sentry if `SENTRY_DSN` is set |

Nobody buckets counts, nobody ships a support bundle, and only MLflow honours `DO_NOT_TRACK` and detects CI. We take MLflow's opt-out rules (fail closed, CI list), Opik's startup notice, Laminar's "no IP stored", and avoid Langfuse's email domains and licence key.

## Architecture

```
                         ┌──────────────── cetra serve ─────────────────┐
 tracing events/spans ──►│ stdout fmt layer        (log.level)           │──► stdout (as today)
                         │ ring buffer layer       (last 2000 lines)     │──► admin.sock debug_info ──► cetra debug-bundle
                         │ OTLP logs layer   ┐                           │
                         │ OTLP traces layer ┘ (self_monitoring.level)  │──► operator's OTLP/HTTP endpoint
                         │ crash layer (ERROR callsites) + panic hook    │──► spool ──► telemetry.cetra.sh/v1/crash  (opt-in)
                         │ heartbeat task (daily)                        │──► telemetry.cetra.sh/v1/heartbeat (opt-out)
                         └───────────────────────────────────────────────┘
```

`init_tracing` moves from `tracing_subscriber::fmt().init()` to one `tracing_subscriber::registry()` with per-layer filters. The stdout layer keeps its formats and its filter (`log.level` plus the `rmcp=warn` cap); its JSON form turns off `with_current_span` and `with_span_list`, so the spans this track adds do not change stdout lines.

### Where the code goes

- `crates/server/src/observability/` (new module): the OTLP exporters and pipeline, the ring buffer layer, the crash layer and panic hook, the heartbeat, the switchboard (who may send what) and the redacted config view. The server owns it because the heartbeat reads `meta.db` and the debug info reads storage. The existing `telemetry.rs` (Prometheus) keeps its name and role.
- `crates/server/src/observability/subscriber.rs` assembles the subscriber (`subscriber::init`), which the CLI calls in place of its `init_tracing`; the `debug-bundle` subcommand lives in `crates/cli/src/debug_bundle.rs`.
- `crates/server/src/admin_socket.rs`: a `debug_info` command (additive within protocol 1, D-R-36). The socket is backup-specific today (`parse` names `backup`, `AdminResponse::Done` carries a `BackupReport`), so M3 generalizes the request parsing and adds a response variant, with a response size cap.
- `crates/server/src/config.rs`: the `[self_monitoring]` and `[telemetry]` tables; `CETRA_TELEMETRY` joins the filtered names; `self_monitoring.otlp_headers` joins the secret paths.
- No change to `cetra-core`, to `cetra-storage`'s public API beyond reading the existing `StorageStats`, or to `meta.db`'s schema (the instance id and heartbeat time are `instance_settings` rows, listed in spec 04).

### Configuration

```toml
[self_monitoring]                       # 1. export to the operator's endpoint (off without an endpoint)
otlp_endpoint = "https://otlp.example.com/otlp"   # base URL; cetra appends /v1/logs and /v1/traces
otlp_headers = "authorization=Basic abc…"        # secret; k=v,k=v (OTEL_EXPORTER_OTLP_HEADERS syntax)
logs = true                             # export log records
traces = true                           # export spans
level = "warn"                          # EnvFilter of the exported logs, rmcp capped as for stdout
trace_sample_ratio = 0.1                # 0.0–1.0 of root spans; children follow their root
resource = "deployment.environment=prod"          # extra resource attributes, k=v,k=v

[telemetry]                             # to us
heartbeat = true                        # 5. daily anonymous heartbeat
crash_reports = false                   # 3. opt-in crash reports
endpoint = "https://telemetry.cetra.sh" # for tests and egress proxies

[analytics]                             # 6. per-user product events to PostHog (off when unset; set on our cloud cells)
posthog_host = "https://eu.i.posthog.com"
posthog_key = "phc_…"                   # secret
```

Env forms follow the loader (`CETRA_SELF_MONITORING__OTLP_ENDPOINT`, …); `resource` and `otlp_headers` are strings so that their env forms work. `CETRA_TELEMETRY` is not the `[telemetry]` table: it is read before the loader and filtered out of the `CETRA_` mapping, like `CETRA_CONFIG` (D-TL-11). `otlp_endpoint`: `http` or `https`, no userinfo, no query; its path is kept and `/v1/logs` or `/v1/traces` appended. `telemetry.endpoint`: `https`, or `http` only to a loopback address (tests). Every key goes into the configuration reference page; spec 12's "cetra has no telemetry key", D-W-18's "`telemetry.*` refused", spec 12 § Self-monitoring and D38/D-N-33's "OTel self-traces stay deferred" are amended (D-TL-1).

### 1. Self-monitoring over OTLP

**Mechanism (D-TL-2).** The OpenTelemetry Rust SDK 0.33.1, the family of the `opentelemetry-proto` 0.33.1 the workspace already pins: `opentelemetry_sdk` (batch processors, sampler, resource), `opentelemetry-appender-tracing` 0.33.1 (tracing events → log records) and `tracing-opentelemetry` 0.34.0 (tracing spans → spans). **Not** `opentelemetry-otlp`: its exporter reads the host's `OTEL_EXPORTER_OTLP_HEADERS`, `_COMPRESSION` and `_TIMEOUT` and merges them into ours, so a key another program set for its own exporter could be sent to the operator's endpoint, and its HTTP clients bring `reqwest` 0.13 next to the workspace's 0.12.28. Instead cetra has two small exporters (`SpanExporter`, `LogExporter`) that convert a batch with `opentelemetry-proto`'s `transform` functions (`group_spans_by_resource_and_scope`, `group_logs_by_resource_and_scope`), encode it with `prost`, gzip it with the workspace's `flate2`, and POST it with the workspace's `reqwest` 0.12. The SDK's batch processors run on their own thread, so an exporter hands each request to the server's tokio runtime and waits for the result. The resource is built with `Resource::builder_empty()` and every batch, sampler and limit setting is given explicitly, so no `OTEL_*` variable changes cetra's export. Rejected: `opentelemetry-otlp` (above); gRPC (Sentry accepts HTTP only); a fully custom pipeline (batching and the span bridge rewritten).

**Signals.** Logs: every `tracing` event that passes `self_monitoring.level` (default `warn`: errors and warnings; an operator who wants request lines sets `info`), as an OTLP log record with the event's fields. Traces: the spans of § 2, sampled at `trace_sample_ratio` (default 0.1). An event inside a sampled span is also recorded on the span with its fields; an ERROR event sets the span's status to error (`tracing-opentelemetry`'s `with_error_events_to_status`), so a traces-only destination (another cetra, D-G-40) still sees which spans failed. Thread names and ids are not recorded. Panics become ERROR events (§ Panics).

**Resource.** `service.name = "cetra"`, `service.version`, `service.instance.id` (a random id per process start, not the heartbeat's instance id), `host.arch`, `os.type`, plus `self_monitoring.resource`. No host name unless the operator adds one.

**Privacy (D-TL-3).** The export carries what stdout carries. `AGENTS.md` already forbids payload content, secrets, keys and emails in logs, and the export applies the same `rmcp=warn` cap. The operator chose the destination, so this is no new exposure; the docs page says so. A test exports to an in-process OTLP receiver and checks that records carry the event's fields and nothing from request bodies.

**Feedback loops.** The export filter drops the exporter's own dependencies (`opentelemetry*`, `hyper*`, `reqwest`, `h2`, `tower`, `rustls`), so exporting never logs into itself. Export failures are logged to stdout at `warn`, at most once a minute. A cetra destination ingesting this server's own export creates one ingest span per export batch, sampled at the same ratio: a bounded trickle, and the docs say to point at another instance.

**Lifecycle.** The pipeline is built right after the config loads, before the storage and `meta.db` open, so startup errors are exported. At shutdown it is flushed last, on a blocking thread with a 2 s timeout, both on the clean path and on the error exit of `cetra serve`.

**Egress.** The endpoint is operator configuration, like identity providers: not passed through the SSRF guard (D166), so an in-cluster collector on a private address works. Redirects are not followed; 10 s request timeout.

### 2. Server spans (traces)

cetra has no `tracing` spans today. This track adds spans where an operator wants latency and failure:

| Span | Attributes |
|---|---|
| `http.request` | method, matched route template (D150), status code |
| `otlp.export` (HTTP and gRPC trace ingest; logs and metrics requests get none) | protocol, span count, byte size, outcome |
| `storage.flush`, `storage.compaction`, `storage.gc` | table, level, bytes, files, outcome |
| `query` (read API, SQL console, MCP) | kind, rows, outcome; never SQL text |
| `eval.job`, `alerts.evaluate`, `alerts.deliver`, `deletion.apply`, `retention.pass`, `issues.pass`, `backup` | ids, kind, outcome |

The rule (D-TL-4): span names and attribute keys are constants; values are ids, counts, enums and durations, never names, URLs, SQL, emails or payload; spans are built with `tracing::info_span!` and explicit fields, never `#[instrument]` (it records every argument). The exact list and attribute names are settled in M2's plan from the code of the day. Spans cost little when no layer records them.

### 3. Crash reports to us (opt-in)

**Switch.** `telemetry.crash_reports = true`. Off by default; the switchboard can force it off (§ The switchboard).

**What a report covers (user).** Every panic, and every ERROR-level `tracing` event, grouped by code location. At most one report per location per hour (later occurrences add to its `count`), at most 20 reports per hour in all.

**What a report contains (user: code-defined facts only, D-TL-5).** A fixed Rust type, serialized as JSON:

| Field | Example | Source |
|---|---|---|
| `schema` | `1` | constant |
| `instance_id` | UUID | added by the sender from `meta.db` (§ Instance id); absent if `meta.db` never opened |
| `version`, `target`, `profile` | `0.4.0`, `x86_64-unknown-linux-musl`, `release` | build constants |
| `deployment` | `docker` / `kubernetes` / `binary` | file and env probes |
| `uptime` | `"1h-1d"` | bucket |
| `kind` | `panic` / `error_event` | |
| `location` | `crates/storage/src/wal.rs:412:9` | panic `Location` or event callsite; path cut by the rule below |
| `target`, `module` | `cetra_storage::wal` | callsite metadata |
| `fields` | `["error", "project_id"]` | field **names** of the callsite, never values |
| `backtrace` | `["cetra_storage::wal::Wal::sync", …]` | function names, demangled, hash suffix removed, at most 48 frames, runtime frames (`std`, `core`, `tokio`) above the first cetra frame collapsed |
| `count`, `first_hour`, `last_hour` | `3`, Unix hour | dedup window, truncated to the hour |

**Path rule.** A workspace path (`crates/…`) is kept as is. A path containing `/.cargo/registry/src/<index>/` or `/.cargo/git/checkouts/` keeps only what follows (`arrow-55.0.0/src/…`). Anything else keeps its file name only. So a build machine's home directory never appears.

**Backtraces.** Release archives are stripped of DWARF but keep the symbol table (D-N-16, `tools/packaging/dist.sh`), so frames have function names but no file or line; the report's `location` gives the line of the panic or event itself. Frames are read with the `backtrace` crate (structured frames; `std::backtrace` has no stable frame API). Function names are code-defined; generic arguments are type names and are kept.

**Never:** the panic message, any field value, error `Display`/`Debug` text, thread names, host names, paths outside the rule, environment, config, exact timestamps. Rust's panic messages embed data (`unwrap()` prints the error's `Debug`, a bad string slice prints the string), so the message is dropped entirely, not scrubbed; `fmt::Arguments::as_str` is unreachable from a layer, so even literal messages are left out. Tests plant an `sk-cetra-` key, an email and a prompt in a panic message and in event fields, then assert the spool file and the request body contain none of them; a golden test fixes the field set.

**Delivery (D-TL-12).** The panic hook and the crash layer write the report synchronously to `<data_dir>/telemetry/spool/` (at most 50 files of 64 KiB; oldest dropped first), so a panic that ends the process is still reported. A sender task posts spooled reports to `<endpoint>/v1/crash` at startup and every minute, adds the instance id, deletes a report on 2xx or 4xx, keeps it on 5xx or a network failure, and drops reports older than 7 days. When crash reports are off, nothing is spooled and an existing spool is deleted at startup. Backups do not copy the spool.

**Panics.** The hook chains the default hook (stderr keeps the message, as today), emits an ERROR `tracing` event `panic` with the location (stdout and OTLP see it), and, when crash reports are on, spools a `panic` report. The crash layer ignores that `panic` event, so a panic is reported once. Task panics that tokio catches go through the same hook. The hook is installed before the config loads; until the switchboard has run, it does not spool.

### 4. `cetra debug-bundle`

```
cetra debug-bundle [--config <file>] [--output <file>]
```

Writes `cetra-debug-<UTC timestamp>.json` (or `--output`), prints its path and "review it before you share it". One JSON document, at most 8 MiB. The command never contacts the network.

- `build`: version, target, profile, UI embedded.
- `host`: OS, kernel release, arch, CPU count, total memory, deployment kind, container memory limit when readable.
- `config` (D-TL-6): the merged configuration sources (defaults, file, env) as a key tree. A value is shown only for keys on an allow-list of non-secret keys; every other leaf reads `"<redacted>"`, so a future secret key is redacted until someone allow-lists it. A test checks that every key of the configuration reference page is classified, and plants a value in every secret key (`encryption_key`, `license_key`, `smtp_url`, `init.*` passwords and keys, `*.client_secret`, `otlp_headers`) and every email key (`init.admin_email`, `email_from`), asserting none appears. Host names, bucket names and paths are allow-listed: the user reviews the file. When the config does not load, the bundle is still written: `config_error` holds the failing key path and the error kind, never the error text (figment and serde errors can quote values).
- `server`: when `cetra serve` runs on the same data directory, the `debug_info` reply over `admin.sock`: uptime, the last 2000 log lines (the ring buffer: JSON lines at the stdout filter, at most 2 MiB), `StorageStats`, catalog file counts and bytes per table and level, WAL and memtable sizes, the `/readyz` checks, `meta.db` counts (orgs, projects, users, keys, rules), the self-monitoring and telemetry state (on or off, and why), and the heartbeat payload as it would be sent. Log lines are payload-free by the `AGENTS.md` rules; as a second guard the bundle replaces anything shaped like an email or a cetra key (`sk-cetra-`, `pk-cetra-`, `pat-cetra-`) with `<redacted>`. When the server is not running: `"not_running"`, the sizes and format versions of `meta.db`, `catalog.db` and the WAL directories read from disk without opening the storage engine, and a hint to attach `docker logs` or `journalctl` output.

`just docs-gen` regenerates the CLI reference.

### 5. Heartbeat (on by default, user)

**Payload (D-TL-8).** A fixed type, JSON, posted to `<endpoint>/v1/heartbeat` with `User-Agent: cetra/<version>`:

| Field | Values |
|---|---|
| `schema` | `1` |
| `instance_id` | UUID |
| `version`, `target`, `profile`, `deployment` | as in crash reports |
| `storage` | `file` / `s3` |
| `uptime`, `install_age` | buckets |
| `orgs`, `projects`, `users` | buckets `0`, `1`, `2-5`, `6-20`, `21-100`, `101-1000`, `1000+` |
| `spans_per_day`, `scores_per_day` | yesterday's totals from `usage_daily`, buckets `0`, `<1k`, `<10k`, `<100k`, `<1M`, `<10M`, `<100M`, `100M+` |
| `features` | booleans: email (SMTP) on; sign-in with GitHub, Google or OIDC configured; alert rules exist; online eval rules exist; datasets exist; prompts exist; annotation queues exist; self-monitoring on; crash reports on |

Never: emails, domains, names, URLs, host names, licence keys or licence state, org or project ids, exact counts. The relay sees the source address of the request, as any server does; it must not store it (TL-Q1).

**Schedule.** The first heartbeat 10 minutes after start (plus up to 10 minutes of jitter), then every 24 h; `instance_settings['telemetry.last_heartbeat_at']` keeps restarts from sending more than one a day. Its own HTTP client: no redirects, 10 s timeout, no retry until the next tick, failures logged at `debug` (an air-gapped install must not get a daily warning).

**Startup notice.** One line at `info` under a dedicated target that the log filter always lets through (like the `rmcp` cap, but raising), so it shows at `log.level = "warn"` too: the heartbeat is on, what it sends, the instance id, the docs URL and how to turn it off; or that it is off, and why. Crash reports get the same line.

### 6. Product analytics (user)

The user wants to know what people do in the product: which features they use and how. One typed event catalog feeds two sinks (D-TL-14):

```
 web UI ── POST /api/v1/analytics/events ──┐
                                           ├─► catalog check ─┬─► PostHog sender (only when [analytics] is set: our cloud cells)
 API handlers ── analytics::record(...) ───┘                  └─► feature counters ─► heartbeat `usage` buckets (self-hosted)
```

**The catalog (D-TL-15).** One Rust enum, one variant per event, with a dotted append-only name (`sql_console.run`, `evaluator.created`, `playground.run`, `dataset.imported`, `dashboard.edited`, `trace.viewed`, `page.viewed`, …) and typed properties: enums, booleans, counts, durations, and our entity ids (project id). Never names, free text, URLs, filter values, attribute keys or payload. `page.viewed` carries the router's route template (`/orgs/$org/projects/$project/traces`), never the URL. The list is written in M6's plan from the pages and routes of the day (about 30–40 events) and is the same idea as the audit log's typed catalog (D-EA-4). A golden test fixes the names and property keys, so every addition shows in review.

**Web events go through the server.** The UI sends catalog events to `POST /api/v1/analytics/events` on its own origin (session cookie, at most 50 events per request, answered `204` whatever happens). The server drops a name or property the catalog does not know, adds the user id and org id from the session (the browser cannot claim another user), and hands the event to the sinks. Consequences: no third-party script in the UI, the CSP stays `default-src 'self'`, no cookie of ours or PostHog's is set, ad blockers do not distort the numbers, and autocapture and session replay are impossible by construction. A request carrying `DNT: 1` or `Sec-GPC: 1` is dropped for the PostHog sink (it still counts toward the anonymous feature counters). The UI sends nothing when `GET /api/auth/config` says analytics is off: when neither the PostHog sink nor the heartbeat runs.

**Server events.** API handlers record the events whose truth is on the server (an evaluator created through the API or MCP, an export downloaded, a backfill started) with the same `record` function, so an action counts once whether the UI, the CLI, the SDK or an agent did it.

**PostHog sink (cloud).** `[analytics]` with `posthog_host` (e.g. `https://eu.i.posthog.com`) and `posthog_key` (a secret path); off when unset. A self-hosted operator may point it at their own PostHog; nothing in the binary names ours. Each event goes to PostHog's capture API in batches (every 10 s or 100 events, bounded queue of 10 000, dropped when full, failures logged at `debug` once a minute): `distinct_id` = the user's UUID, `$groups` = `{ "org": <org id> }`, the catalog properties, `$ip` = null, `$process_person_profile` true. No email, name or org name is ever sent; PostHog person and group properties are limited to the user's org role and the org's plan name. Our cloud's PostHog is self-hosted on our ovh-us server (Virginia) and reached over the tailnet.

**Feature counters (self-hosted).** Every catalog event, from any source, increments an in-memory counter per event name; counters are saved hourly to `instance_settings['telemetry.usage_counts']` and reset when a heartbeat is sent. The heartbeat gains `usage`: for each event name, the count since the last heartbeat in the buckets `0`, `1-9`, `10-99`, `100-999`, `1000+`. No users, orgs, sequences or timings. Turned off with the heartbeat (the switchboard).

### Details settled while planning (coordinator, 2026-11-23)

Smaller choices the milestone plans needed, within the approved design:

- **Buckets.** `uptime` (time since the process started, the same in heartbeats and crash reports): `<1h`, `1h-1d`, `1d-7d`, `7d-30d`, `30d+`. `install_age` (from the oldest `users.created_at`; no user yet reads `<1d`): `<1d`, `1d-7d`, `7d-30d`, `30d-90d`, `90d-1y`, `1y+`. The relay (TL-Q1) accepts exactly these strings and those of § 5.
- **Counts.** Active orgs, active projects, users not disabled.
- **Sign-in features.** Three booleans, `github_sign_in`, `google_sign_in`, `oidc_sign_in`, from the server config only; per-org SSO (`ee`) is not read. `crash_reports_on` joins `features` in M5.
- **Heartbeat bookkeeping.** `telemetry.last_heartbeat_at` is written only after a 2xx answer.
- **`telemetry.endpoint` over `http`** only to a loopback IP literal (`127.0.0.0/8`, `::1`); `localhost` is refused, since a name can resolve elsewhere. Test configs set `http://127.0.0.1:9`.
- **The switchboard's environment** is read once by the config loader (`CETRA_TELEMETRY`, `DO_NOT_TRACK`, the CI variables) and kept beside the config, never as config keys; a bad `CETRA_TELEMETRY` fails every command that loads the config.
- **`otlp_headers` and `resource`:** at most 32 entries of at most 8192 bytes; duplicate names refused; header values percent-decoded (Grafana's `Basic%20…` snippets work as pasted); `content-type`, `content-encoding`, `content-length`, `host` and `user-agent` refused (cetra sets them); a `resource` key may override cetra's own attributes. Headers and resource without an endpoint are validated and unused.
- **`otlp_endpoint`** ending in `/v1/logs` or `/v1/traces` is refused (cetra appends the signal path).
- **Shutdown.** The 2 s export flush sits outside spec 12's 60 s bound, like the final meta writes. The error exit of `cetra serve` emits an ERROR `cetra serve failed` event (on stdout too) so startup errors are exported.
- **`cetra init`'s local server** (`node-cli`) runs `cetra serve`, so the heartbeat applies there too; the Telemetry page says so.

- **Crash reports (M5).** The build triple keeps the key `target`; the callsite target is `event_target` (absent for panics, as is `module`). The backtrace drops the leading run of reporter and runtime frames (`std`, `core`, `alloc`, `tokio`, `backtrace`, `tracing*`, C symbols) with no marker, then keeps up to 48 frames. Dedup: the first occurrence of a location in an hour is spooled at once with count 1; repeats and capped occurrences are counted in memory and spooled as one report after the hour (lost if the process exits first); panics and ERROR events share the dedup and the cap. The `message` field name is left out of `fields`. A 1xx or 3xx answer is kept like a 5xx, and a sending pass stops at the first kept answer. Caps: 512 bytes per function name and for `location`, 256 for targets, modules and field names, 32 field names, 63 KiB per spooled body (dropped backtrace, then the report, when larger). `features.crash_reports_on` is the switchboard's decision. ERROR events from dependencies are reported too. The instance id is added when a report is sent, and left out if it cannot be read.

- **Debug bundle (M3).** `config` shows the keys the user set (file and environment) plus the names of the `CETRA_*` variables present; serde defaults are left out. Shown values include domains, the OIDC issuer and client id, and the `init` org and project names (with host names, bucket names and paths). `config_error` carries the key path, the error kind and its static reason, never figment or serde text. Catalog file counts come from a read-only query of `catalog.db`, as backups already read versions. `--output -` writes to stdout, for distroless containers. The golden server scenario records one `debug_info` exchange. Host facts use `sysinfo` (already a workspace dependency).

- **Product analytics (M6).** The `analytics` flag of `GET /api/auth/config` is true when either sink runs (the PostHog sink or the heartbeat's feature counters), so self-hosted installs count page views and UI-only events too; the server still sends per-user events to PostHog only when `[analytics]` is set. A web event carries the leaf route's org and project slugs, which the server resolves with the normal access checks (a resolution failure drops only the event's PostHog delivery; the feature counter still counts it) and never forwards. The PostHog sink depends on `[analytics]` and restore mode only, not on the switchboard; the counters count only while the heartbeat runs (`analytics::record` skips them when it is off) and are saved hourly and after an accepted heartbeat, so a restart can lose up to an hour. Events without a user (API keys, the control plane) go to the counters only. Outside a request the DNT/Sec-GPC signal counts as opted out. `org_role` is sent only when known without an extra query, and org group properties at most once an hour per org. `trace.viewed` counts the server's trace-detail reads, router preloads included.

- **Server spans (M2).** Events recorded on spans pass the export filter (`self_monitoring.level`, the `rmcp` cap, the dropped exporter crates). An ERROR event marks its span failed (`tracing-opentelemetry` 0.34 does it whatever `with_error_events_to_status` says; the flag stays off, so a message-less `error` field at WARN does not mark the span or copy its text into the status). `/healthz` and `/readyz` get no `http.request` span. Read API routes get no separate `query` span (the request span with its route template covers them); the SQL console and MCP tool calls do. Spans cover units of work: one per deletion, alert rule, delivery and issue candidate (`issues.candidate`, not `issues.pass`). `trace_sample_ratio = 0` builds no span layer. Trace ids in log records are out of scope. `http.request` ends at the response head; a 5xx marks it failed. `trace_sample_ratio` is a validated newtype, since `ServerConfig` derives `Eq`.

### The switchboard (D-TL-11)

One function decides, at startup, what may go to us. The first match wins:

1. `CETRA_TELEMETRY` (case-insensitive) is `off`, `false` or `0` → heartbeat and crash reports off. Empty is ignored. Any other value is a startup error naming the accepted values, so a typo never leaves telemetry on (`on` is an error too: the config turns things on).
2. `DO_NOT_TRACK` is non-empty and not `0` or `false` (case-insensitive) → both off.
3. `restore_mode = true` → both off (no outbound work, D-N-25). Self-monitoring stays on in restore mode: it goes to the operator's own endpoint.
4. Heartbeat only: a debug build, or a CI environment (`CI` non-empty and not `false`/`0`; or any of `GITHUB_ACTIONS`, `GITLAB_CI`, `BUILDKITE`, `CIRCLECI`, `JENKINS_URL`, `TF_BUILD`, `TRAVIS`, `TEAMCITY_VERSION`, `BITBUCKET_BUILD_NUMBER`, `CODEBUILD_BUILD_ID`, `DRONE`, `APPVEYOR`, `SEMAPHORE` non-empty) → heartbeat off.
5. Otherwise the config: `telemetry.heartbeat` (default `true`) and `telemetry.crash_reports` (default `false`).

An error while deciding (other than rule 1's) means off. Self-monitoring is not part of the switchboard: only the operator's config turns it on.

Release binaries running in CI containers do not see the host's `CI` variable, so every packaging smoke test (`just image-smoke`, `compose-smoke`, `helm-smoke`, `install-smoke`, `tools/e2e/run.sh`) sets `CETRA_TELEMETRY=off` (M4).

### Instance id (D-TL-9)

A random UUIDv4 in `instance_settings['telemetry.instance_id']`, created on first use with an insert that keeps an existing row (no race between the heartbeat and the crash sender). Not a UUIDv7: its timestamp would reveal the install time, which the heartbeat sends only as a bucket. A restored backup keeps the id (it is the same instance); a drill copy runs in restore mode and sends nothing; a data directory copied by hand shares the id, which the docs mention. The id is used for nothing else, never joined with a licence, and shown in the startup notice and the debug bundle so a user can ask us to delete its data.

### Licence separation (D172 stays true)

The heartbeat and crash reports never read licence state, the `ee` modules never call the telemetry code, and `telemetry.cetra.sh` is not a licence endpoint. Turning telemetry off changes nothing about licences, and licences never turn telemetry on or off (unlike Langfuse).

### New dependencies (versions re-checked by each plan)

| Crate | Version | Why |
|---|---|---|
| `opentelemetry` | 0.33.1 (already in `Cargo.lock` through `opentelemetry-proto`) | API types |
| `opentelemetry_sdk` | 0.33.1, features `trace`, `logs` (no `rt-tokio`) | batch processors, sampler, resource |
| `opentelemetry-appender-tracing` | 0.33.1 | events → log records |
| `tracing-opentelemetry` | 0.34.0, default features off | spans → spans |
| `opentelemetry-proto` | 0.33.1, add features `trace`, `logs` | `transform` for the exporters |
| `backtrace` | 0.3.77 | structured frames for crash reports (M5) |

All are MIT or Apache-2.0 (on the `deny.toml` allow-list).

## Decisions

| ID | Decision | Rejected |
|---|---|---|
| D-TL-1 | `[self_monitoring]` and `[telemetry]` config tables; amends D-W-18, spec 12's "no telemetry key" and D38/D-N-33's deferral of self-traces. | Reading `OTEL_*` variables: a host that sets them for its own apps would change cetra's export by accident. |
| D-TL-2 | The OpenTelemetry Rust SDK 0.33.1 with cetra's own two OTLP/HTTP exporters on `opentelemetry-proto`'s `transform`, `prost`, `flate2` and `reqwest` 0.12. | `opentelemetry-otlp` (reads `OTEL_EXPORTER_OTLP_*`, brings `reqwest` 0.13); gRPC; a fully custom pipeline. |
| D-TL-3 | The export carries what stdout carries, with the same caps; logs default to `warn`, traces to 10 %. | A second allow-list for the same lines. |
| D-TL-4 | Server spans with constant names and keys; values are ids, counts, enums, durations. | `#[instrument]`, which records every argument. |
| D-TL-5 | Crash reports carry code-defined facts only: no message, no field values, function names without lines (user). | Scrubbed messages (a scrubber misses data); location only (too little). |
| D-TL-6 | `debug-bundle` is one local JSON file; config values shown by allow-list; data from the running server over `admin.sock`. | A deny-list (misses future secrets); a tarball; opening the storage engine next to a running server. |
| D-TL-7 | Incoming `traceparent` is ignored. | Joining callers' traces: any client could attach our spans to its trace. |
| D-TL-8 | Daily heartbeat on by default, bucketed counts and feature booleans (user). | Exact counts and email domains (competitors); opt-in. |
| D-TL-9 | Instance id: random UUIDv4 in `instance_settings`. | UUIDv7 (reveals install time); a file outside `meta.db` (lost by backups). |
| D-TL-10 | Crash reports and heartbeats go to `telemetry.cetra.sh`, a relay track Q runs (user). | The control plane; a vendor DSN in the binary. |
| D-TL-11 | One switchboard: `CETRA_TELEMETRY`, `DO_NOT_TRACK`, restore mode, then CI and debug builds for the heartbeat, then config; fail closed. | Licence-dependent rules; no CI detection. |
| D-TL-12 | Crash reports are spooled to disk before sending. | An in-memory queue: a panic that ends the process would never be reported. |
| D-TL-13 | Our errors go to a self-hosted GlitchTip (user, 2026-11-24; replaces Sentry EU); heartbeats stay in the relay's own storage; the relay sends no instance id to GlitchTip. | Sentry EU (OTLP logs land in Logs, not Issues; 30-day retention on the free plan); a cetra project only (no log records, weak triage); SigNoz, Grafana Cloud; a product-analytics SaaS for heartbeats. |
| D-TL-14 | Product analytics (user): one event catalog, two sinks: per-user events to PostHog when `[analytics]` is set (our cloud), bucketed feature counts in the heartbeat (self-hosted). Web events go through the server. | posthog-js in the UI (third-party script, CSP change, cookies, autocapture and replay would capture prompts shown on screen); per-user analytics on self-hosted installs. |
| D-TL-15 | A typed catalog with dotted names and code-defined properties; page views by route template; the user and org from the session; `DNT`/`Sec-GPC` honoured. | Free-form event names and properties (unreviewable, leak user text). |

## Requests to track Q

Written to `side-session-notes/…/tl-requests-for-q.md` when the design is approved.

- **TL-Q1 — the relay at `telemetry.cetra.sh`.** `POST /v1/heartbeat` and `POST /v1/crash`, JSON bodies of D-TL-8 and D-TL-5 (schema 1; unknown schema → 400), bodies ≤ 64 KiB, 2xx with an empty body; rate limits kept in memory by source address (never stored): 60 heartbeats and 120 crash reports an hour per address, 2 000 crash reports an hour in all, `429` above them (D-Q-156–D-Q-167). cetra sends far fewer: one heartbeat a day, at most 20 crash reports an hour per instance, and it keeps a report answered `429` for a later pass. Heartbeats stored without the address (one row per `instance_id` per day, kept 13 months). Crash reports forwarded to our error destination, kept 90 days. A way to delete an instance's data on request.
- **TL-Q2 — cells.** Each cell sets `self_monitoring.otlp_endpoint` to the relay's OTLP log intake over the tailnet (`http://<ops VM Tailscale IP>:<port>`) and `otlp_headers` to its own bearer token (from a secret), `self_monitoring.traces = false` (GlitchTip does not take OTLP traces yet), `self_monitoring.resource = "cetra.cell=<name>"`, `telemetry.heartbeat = false` (a cell is ours) and `telemetry.crash_reports = false` (its errors already reach us through self-monitoring).
- **TL-Q3 — the control plane** sends its own OTLP logs to the same relay intake with its own token, so cloud errors are in one place.
- **TL-Q4 — PostHog.** A self-hosted PostHog on our ovh-us server (Virginia), reached over the tailnet (user, 2026-11-24; replaces PostHog Cloud EU); each cell sets `analytics.posthog_host` and `analytics.posthog_key` (from a secret); the control plane's own pages, if they want events, use the same catalog names.
- **Destination: self-hosted GlitchTip (user, 2026-11-24, D-TL-13; replaces Sentry EU).** GlitchTip runs with Postgres on our monitoring VM (Hetzner, Germany), so no new processor. The relay forwards crash reports to it as events grouped by `[kind, location]`, without the `instance_id`; heartbeats stay in the relay's SQLite. GlitchTip 7.0.0 stores OTLP log records in its Logs view but never turns them into issues, and takes no OTLP traces, so the cells and the control plane send their OTLP logs to an intake on the relay (TL-Q2, TL-Q3): the relay forwards every record to GlitchTip's Logs and also sends each ERROR record as a Sentry-format event grouped by `[service.name, target, code location]`, which becomes an issue with email alerts (user, 2026-11-24). Cells keep `self_monitoring.traces = false` until GlitchTip takes traces. Sentry EU was dropped: its OTLP log records go to Logs, not Issues, and its free plan keeps errors 30 days. The binary only knows an OTLP endpoint and the relay's hostname, so none of this changes the code.

## Milestones

Order: M1 and M4 first, in parallel; then M2 (after M1), M3 and M5 (after M1 and M4) and M6 (after M4), in parallel where their files allow.

### M1 — Self-monitoring: logs and errors over OTLP

Delivers: the `[self_monitoring]` config (logs only: `otlp_endpoint`, `otlp_headers`, `logs`, `level`, `resource`); the registry-based `init_tracing`; the OTLP log exporter; the panic hook's ERROR event; the flush on shutdown; docs (monitoring page section with Sentry, Grafana Cloud and Collector examples; configuration reference; CHANGELOG); spec 12 and the amended decisions. Depends on nothing. Leaves out: spans (M2), anything sent to us.

### M2 — Server spans and trace export

Delivers: the spans of § 2, the span exporter, `self_monitoring.traces` and `trace_sample_ratio`, error status from ERROR events, the stdout JSON layer without span fields; docs updated. Depends on M1.

### M3 — `cetra debug-bundle`

Delivers: the log ring buffer, the allow-list config view, the generalized admin socket and its `debug_info` command, the `cetra debug-bundle` command (running, not running, invalid config), the telemetry state and heartbeat preview, the CLI reference, a "Reporting a bug" docs page and a GitHub issue template that asks for the bundle. Depends on M1 (the subscriber assembly) and M4 (facts, switchboard, heartbeat payload).

### M4 — The switchboard and the heartbeat

Delivers: `[telemetry]` (`heartbeat`, `endpoint`), `CETRA_TELEMETRY` and `DO_NOT_TRACK`, CI and debug-build detection, the instance id, the heartbeat payload, schedule and startup notice, `CETRA_TELEMETRY=off` in every packaging smoke test and the e2e script, a "Telemetry" docs page listing every field and how to turn it off, a README line, CHANGELOG with an upgrade note (a new outbound request on by default), spec 04's `instance_settings` keys, and TL-Q1/TL-Q2/TL-Q3 in the notes folder. Depends on nothing (runs alongside M1; its startup-notice filter edits M1's subscriber). Real delivery needs Q's relay; until then failures are silent at `debug`. Leaves out: crash reports.

### M5 — Opt-in crash reports

Delivers: `telemetry.crash_reports`, the report type, the crash layer, the panic hook's spool, the sender, the caps and dedup, the path rule, the leak tests, the Telemetry docs page section listing every field. Depends on M1 (panic hook) and M4 (switchboard, instance id, endpoint).

### M6 — Product analytics

Delivers: the event catalog, `analytics::record` in the API handlers, `POST /api/v1/analytics/events` and its OpenAPI entry, the `analytics` flag in `GET /api/auth/config`, the web client (page views by route template and UI-only events), the PostHog sink and `[analytics]` config, the feature counters and the heartbeat's `usage` field, the leak and golden tests, the Telemetry docs page section, TL-Q4. Depends on M4 (heartbeat, switchboard). Leaves out: dashboards in PostHog (Q).

## Answers (user, 2026-11-23)

The user approved the design with each proposal below.

1. **Naming:** `[self_monitoring]` for the operator's export and `[telemetry]` for what goes to us; `[analytics]` for PostHog.
2. **Heartbeat fields:** the `features` list of § 5 and `deployment` (docker, kubernetes, binary) as written.
3. **Debug bundle and hosts:** host names, bucket names and paths are kept; the user reviews the file.
4. **Export defaults:** exported logs at `warn`, traces at 10 %.
5. **Milestone order:** as § Milestones; independent milestones run in parallel (user: "try to parallelize").
6. **Cloud analytics opt-out:** no account switch for now; `DNT` and `Sec-GPC` are honoured.

## Questions asked during the design (kept for the record)


1. **Naming.** `[self_monitoring]` for the operator's export (spec 12's heading) and `[telemetry]` for what goes to us. OK, or `[otel]` / `[otlp_export]`?
2. **Heartbeat fields.** Is the `features` list right, and is `deployment` (docker/kubernetes/binary) acceptable?
3. **Debug bundle and hosts.** The bundle shows host names, bucket names and paths (the user reviews it before posting). Keep, or redact them too?
4. **Export defaults.** Logs at `warn` and traces at 10 % by default once an endpoint is set (an ingest-heavy server would otherwise export one log line and one trace per OTLP request). OK?
5. **Milestone order.** Debug bundle (M3) before the heartbeat (M4), crash reports (M5) and product analytics (M6). OK?
6. **Analytics opt-out for cloud users.** Besides `DNT`/`Sec-GPC`, should a cloud user get an "Allow product analytics" switch in account settings (proposed: no for now; first-party, pseudonymous, no cookies)?
