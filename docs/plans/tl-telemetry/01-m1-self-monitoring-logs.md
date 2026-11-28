# TL M1 — Self-monitoring: logs and errors over OTLP

**Summary.** This milestone lets an operator send `cetra serve`'s own log events to an OTLP/HTTP endpoint they choose (Sentry, Grafana Cloud, an OpenTelemetry Collector, …). It adds:
- a `[self_monitoring]` table with five keys: `otlp_endpoint`, `otlp_headers`, `logs`, `level`, `resource`;
- a new module, `crates/server/src/observability/`, with three parts:
  - cetra's own OTLP/HTTP protobuf **log** exporter. It implements `opentelemetry_sdk`'s `LogExporter` on `opentelemetry-proto`'s `transform`, `prost`, `flate2` gzip and the workspace `reqwest` 0.12. It hands each request from the SDK's batch thread to the tokio runtime.
  - the registry-based subscriber the CLI installs. The stdout layer behaves as today, and the export layer has its own filter.
  - the panic hook, which adds an ERROR `panic` event that carries the code location and never the message.
- a flush of the export on shutdown, bounded at 2 s on a blocking thread. It runs both on the clean exit and on the error exit of `cetra serve`.
- the docs, the configuration reference, spec 12 and the CHANGELOG.

Nothing goes to us. Spans are M2's work.

**Prerequisites.** The design `docs/plans/tl-telemetry/README.md` is approved (2026-11-23), with D-TL-1 to D-TL-15 in `docs/mvp-decisions.md` § Track TL. Work happens in the worktree `/home/sebastien/travaux/llm-observer/.claude/worktrees/cross-track-review-6a41ad` on branch `claude/telemetry-9e2a24`. Track TL M4, planned in `04-m4-heartbeat.md`, runs in parallel in the same checkout; the coordinator schedules the phases of both (§ Phase graph lists the files each phase touches).

**Read first.**
- `AGENTS.md`, all of it. These sections matter most here:
  - § Rules (no `unwrap`/`expect` without an invariant comment; no panic on input; `thiserror` per crate; dependencies declared once in the root);
  - § Privacy;
  - § Tests (the configuration reference test, the CHANGELOG, the docs pages).
- `docs/plans/tl-telemetry/README.md`:
  - § Architecture, § Where the code goes, § Configuration;
  - § 1 Self-monitoring over OTLP (all of it);
  - § 3, its last paragraph **Panics**;
  - § New dependencies;
  - § Decisions D-TL-1, D-TL-2, D-TL-3;
  - § Milestones M1.
- `crates/cli/src/main.rs`:
  - `serve` (line ~374), `annotate`, `RMCP_DIRECTIVE`, `log_filter`, `init_tracing`, `default_log_format` (lines ~374–440);
  - `async_main` (error printing);
  - the tests `log_filter_caps_rmcp` and `a_debug_filter_drops_rmcp_debug_events` (lines ~1020–1067).
- `crates/server/src/config.rs`:
  - `ALIASES`, `Secret`, `PublicUrl` and its `Deserialize`, `LogConfig`, `ServerConfig`;
  - `map_figment` and `secret_key`, `validate`, `parse_public_url`, `is_http_url`, `deserialize_optional_socket_addr`;
  - `EnvSource`, `split_key`, `is_filtered`, `env_value`, `is_secret_path`;
  - the tests `defaults_match_spec` and `the_configuration_reference_matches_the_loader`, with its helpers `documented`, `probe` and `accepted` (lines ~3580–3757).
- `crates/server/src/serve.rs` lines 1–13: the shutdown budget.
- `crates/server/src/lib.rs`, `crates/server/Cargo.toml`, the root `Cargo.toml` `[workspace.dependencies]`, `deny.toml` `[licenses]`.
- `crates/server/src/test_support/mod.rs`: `ensure_global_subscriber` and `capture_logs` (lines ~1595–1640).
- `crates/server/src/auth/oidc/mod.rs`: `identity_client` (line ~192). It is the model for the HTTP client (no redirects, 10 s, no SSRF guard for operator configuration).
- `crates/server/src/ingest/body.rs` line ~174: the use of `flate2` `GzEncoder` in tests.
- The SDK sources under `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`:
  - `opentelemetry_sdk-0.33.1/src/logs/export.rs`: the `LogExporter` trait and `LogBatch` (`iter()` is public, `len()` is not);
  - `opentelemetry_sdk-0.33.1/src/logs/batch_log_processor.rs`:
    - the thread at line ~469, `export_batch_sync` with `futures_executor::block_on` at line ~681;
    - `shutdown_with_timeout` at line ~307;
    - `BatchConfigBuilder` and `init_from_env_vars` at lines 720–880;
  - `opentelemetry_sdk-0.33.1/src/logs/logger_provider.rs`:
    - `with_resource`;
    - `build()`, which falls back to the env-reading `Resource::builder()` when no resource is given;
    - `shutdown_with_timeout`;
  - `opentelemetry_sdk-0.33.1/src/resource/mod.rs`: `builder_empty`, `with_attributes` (a later value wins);
  - `opentelemetry-appender-tracing-0.33.1/src/layer.rs` lines 355–560: `OpenTelemetryTracingBridge::new`, `on_event`, `EventVisitor`;
  - `opentelemetry-proto-0.33.1/src/transform/logs.rs` lines 1–225: `tonic::group_logs_by_resource_and_scope`;
  - `opentelemetry-proto-0.33.1/src/transform/common.rs`: `tonic::ResourceAttributesWithSchema` and its `From<&Resource>`;
  - `tracing-subscriber-0.3.23/src/layer/mod.rs` line ~1776 (`Layer for Vec<L>`), `src/filter/layer_filters/mod.rs` (`FilterExt::and`), `src/filter/filter_fn.rs` (`filter_fn`).

## What it is, and what it is not

**What works when it's done:**
- With `self_monitoring.otlp_endpoint` set, `cetra serve` exports every `tracing` event that passes `self_monitoring.level` (default `warn`) as an OTLP log record. Each record carries:
  - the event's message as its body;
  - its fields as attributes;
  - its severity;
  - its event name (`event <file>:<line>`, set by the bridge);
  - its target, as the scope name;
  - the resource `service.name`, `service.version`, `service.instance.id` (UUIDv7 per process start), `host.arch`, `os.type`, plus `self_monitoring.resource`.
- The request is protobuf, gzip, `POST <otlp_endpoint>/v1/logs`, with the operator's `otlp_headers`, no redirects and a 10 s timeout.
- No `OTEL_*` environment variable changes the export (proved by a child-process test).
- The export never carries the exporter's own stack (`opentelemetry*`, `hyper*`, `reqwest`, `h2`, `tower*`, `rustls*`, `tokio_rustls`, `webpki`, `want`, `http_body`) or its own failure lines. `rmcp` is capped at `warn`, as for stdout.
- An export failure is logged on stdout at `warn`, at most once a minute, and is never exported.
- Stdout lines are byte-identical to today's, in both formats.
- A panic in `cetra serve`:
  - still prints Rust's default message on stderr;
  - also emits an ERROR event `panic` with `location = file:line:column`, never the message. That event reaches stdout and the export.
- At exit, clean or error, the export is flushed on a blocking thread for at most 2 s. An error exit first emits `cetra serve failed` with the error chain.
- The docs (Monitoring page section with Sentry, Grafana Cloud and Collector examples), the configuration reference, spec 12 and the CHANGELOG are updated.

**Out of scope:**
- Spans, `self_monitoring.traces`, `trace_sample_ratio`, and turning off the JSON span fields: M2.
- The ring buffer layer: M3.
- `[telemetry]`, `CETRA_TELEMETRY`, the switchboard, the instance id and the start-up notice directive: M4.
- The crash layer and the panic hook's spool: M5. This plan leaves a seam for them (§ Interface for later milestones).
- The D38 and D-N-33 notes: done by the coordinator.
- Exporting from any command other than `cetra serve`.
- gRPC export, an OTLP metrics export, and reading `OTEL_*` variables.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-TL-1 | `[self_monitoring]` table (and `[telemetry]`, in M4); no `OTEL_*` variables |
| D-TL-2 | OpenTelemetry Rust SDK 0.33.1 with cetra's own OTLP/HTTP exporter on `opentelemetry-proto` `transform`, `prost`, `flate2`, `reqwest` 0.12; not `opentelemetry-otlp`; every batch setting explicit; `Resource::builder_empty()` |
| D-TL-3 | The export carries what stdout carries, with the same `rmcp=warn` cap; logs default to `warn` |
| D-N-28 | Prometheus `/metrics` stays the metrics interface; `telemetry.rs` untouched |
| D-N-25 | Restore mode: no outbound work, except the operator's own self-monitoring export (design § The switchboard, rule 3) |
| D166 | The endpoint is operator configuration: no SSRF guard, like identity providers |
| D150 | No `axum/tracing` and no `tower-http::TraceLayer` |

**Details settled while planning.** The design's `docs/plans/tl-telemetry/README.md` § "Details settled while planning" (coordinator, 2026-11-23) records these, and this plan applies them:
- `otlp_headers` and `resource`:
  - at most 32 entries and 8192 bytes;
  - duplicate names are refused;
  - header values are percent-decoded;
  - `content-type`, `content-encoding`, `content-length`, `host` and `user-agent` are refused;
  - a `resource` key may override cetra's own attributes;
  - both are validated and unused without an endpoint.
- An `otlp_endpoint` ending in `/v1/logs` or `/v1/traces` is refused.
- The 2 s flush sits outside spec 12's 60 s shutdown bound.
- The error exit emits an ERROR `cetra serve failed` event, which also shows on stdout.

Plan-level choices (implementation detail the design leaves to the plan):
- **Stdout JSON span fields.** M1 keeps the formatter defaults: cetra has no spans yet, and a test proves the output is byte-identical. M2 turns `with_current_span` and `with_span_list` off.
- **Where `self_monitoring.level` is checked.** It is checked when the subscriber is built (TL1.5), as `log.level` is today, not by the config loader. It is checked even when nothing is exported, so a bad value is always a start-up error.
- **Export failures** use the target `cetra_self_monitoring` (a constant), which the export filter drops.
- **Batch settings.**
  - Queue 2048 records, delay 1 s, batch 512, set through `BatchConfigBuilder`'s three setters. A value set there replaces the `OTEL_BLRP_*` one.
  - `OTEL_BLRP_EXPORT_TIMEOUT` exists only behind the SDK feature `experimental_logs_batch_log_processor_with_async_runtime`, which stays off. The 10 s request timeout is the bound.
- **Proxy.** The export client honours `HTTP(S)_PROXY` like `identity_client`: that is not an `OTEL_*` variable, and it is how egress proxies work. In this crate's tests, the client is built with `.no_proxy()`, so a developer's proxy cannot catch the 127.0.0.1 receivers.
- **`host.arch` and `os.type`** use the OTel semantic-convention values (`amd64`, `arm64`, `darwin`, …), mapped from `std::env::consts` (TL1.4).
- **The error chain is exported.** `cetra serve failed` exports `ServeError`'s chain to the operator's endpoint, the same text stderr prints. Its variants carry paths, bind addresses, and storage or meta error kinds. They carry no payload and no secret: `Init` is documented payload-free, and config errors never reach the export, because the pipeline needs the config. This fits AGENTS.md § Privacy.
- **Sentry's EU host.** The docs tell the operator to copy it from their project's DSN, and show `ingest.de.sentry.io` with placeholders.

## Interface for later milestones

These names are stable. M2–M5 plans reference them.

**`crate::config` (TL1.2)**
- `pub struct SelfMonitoringConfig` has these fields:
  - `pub otlp_endpoint: Option<OtlpEndpoint>`
  - `pub otlp_headers: Option<Secret<String>>`
  - `pub logs: bool`
  - `pub level: String`
  - `pub resource: String`
- Its methods:
  - `pub fn logs_endpoint(&self) -> Option<&OtlpEndpoint>`: the endpoint, when `logs` is true.
  - `pub fn exports(&self) -> bool`: whether anything is exported. In M1 that is `logs_endpoint().is_some()`. M2 widens it to traces. M4's heartbeat feature `self_monitoring` reads `config.self_monitoring.exports()`.
- `ServerConfig.self_monitoring: SelfMonitoringConfig`, placed after `log`.
- `pub struct OtlpEndpoint`, with:
  - `pub fn logs_url(&self) -> String`
  - `pub fn host(&self) -> &str`
  - a `Debug` that prints the host only.
- `pub(crate)` items:
  - `fn parse_otlp_endpoint(&str) -> Result<OtlpEndpoint, &'static str>`
  - `fn parse_otlp_headers(&str) -> Result<Vec<(String, String)>, &'static str>`
  - `fn parse_resource(&str) -> Result<Vec<(String, String)>, &'static str>`
  - `fn load_from(Option<&str>, &[(&str, &str)]) -> Result<ServerConfig, ConfigError>`
  - `const MAX_PAIRS`, `const MAX_PAIRS_BYTES`

**`crate::observability` (`mod.rs`, TL1.3–TL1.5)**
- `pub enum ObservabilityError` (thiserror). Later milestones add variants at the end. Its variants and messages:
  - `Client` → `self_monitoring: cannot build the HTTP client`
  - `Header` → `self_monitoring.otlp_headers: invalid header`
  - `Resource` → `self_monitoring.resource: invalid attribute list`
  - `LogLevel(tracing_subscriber::filter::ParseError)` → `log.level: {0}`
  - `ExportLevel(tracing_subscriber::filter::ParseError)` → `self_monitoring.level: {0}`
  - `NoRuntime` → `no tokio runtime to run the export on`
  - `AlreadyInstalled` → `a global tracing subscriber is already installed`

**`crate::observability::subscriber` (TL1.5)**
- `pub fn init(config: &ServerConfig, format: LogFormat) -> Result<Observability, ObservabilityError>`: the CLI's only entry point.
- `pub(crate) fn build(config: &ServerConfig, format: LogFormat, writer: BoxMakeWriter, runtime: Handle) -> Result<(ComposedSubscriber, Observability), ObservabilityError>`.
  - A later milestone adds its layer by pushing one more `BoxedLayer` onto the `layers` vector in `build`, after the stdout layer.
  - If it needs a flush, it adds a field to `Observability` and a step to `Observability::shutdown`.
- `pub(crate) type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;`
- `pub(crate) type ComposedSubscriber = Layered<Vec<BoxedLayer>, Registry>;`
- `pub struct Observability`, with:
  - `pub fn exporting_logs(&self) -> bool`
  - `pub async fn shutdown(self)`
- `pub(crate) const FLUSH_TIMEOUT: Duration` (2 s).
- The filters. All three return `Result<EnvFilter, tracing_subscriber::filter::ParseError>`:
  - `fn capped_filter(level: &str)` (private): `EnvFilter::try_new(level)` plus the `rmcp=warn` directive. It is shared by the two below.
  - `pub(crate) fn log_filter(level: &str)`: the **stdout** filter. It calls `capped_filter`. **M4's TL4.7 edits only this function.** It adds `let notice = NOTICE_DIRECTIVE.parse()?;` and `.add_directive(notice)` after `capped_filter`'s result. That parse returns `ParseError` too, so `?` needs no context and no mapping. The export never sees that directive.
  - `fn export_filter(level: &str)` (private): the **export** filter. It calls `capped_filter` and nothing else.
- `fn export_target_allowed(target: &str, level: Level) -> bool` (private): the dropped prefixes and the `rmcp` cap.

**`crate::observability::panic` (TL1.3)**
- `pub fn install()`: idempotent. The CLI calls it before the config loads.
- `fn report(info: &PanicHookInfo<'_>)` (private): **the seam**. M5 adds the spool call after the `tracing::error!` line, inside the `HookGuard`.
- `pub(crate) const PANIC_EVENT_TARGET: &str = "cetra_server::observability::panic"`. M5's crash layer skips events with this target.
- `pub(crate) fn panic_location(info: &PanicHookInfo<'_>) -> String`

**`crate::observability::otlp` (TL1.4)**
- `pub(crate) const EXPORT_FAILURE_TARGET: &str = "cetra_self_monitoring"`
- `pub(crate) struct LogPipeline`, with:
  - `new(&SelfMonitoringConfig, &OtlpEndpoint, Handle)`
  - `bridge()`
  - `host()`
  - `shutdown(Duration)`
- M2 adds its span exporter beside it in this file.

## Versions

Checked on 2026-11-23 with `cargo info` and the registry sources. The implementer re-checks with `cargo info <crate>` and reports the versions used.

| Crate | Version | Features | Where | Why |
|---|---|---|---|---|
| `opentelemetry` | 0.33.1 (already in `Cargo.lock` through `opentelemetry-proto`) | `default-features = false`, `["logs"]` | root `[workspace.dependencies]`; `cetra-server` | `KeyValue` for the resource. No `internal-logs`, which would route the SDK's own diagnostics through `tracing` |
| `opentelemetry_sdk` | 0.33.1 (already in `Cargo.lock`) | `default-features = false`, `["logs"]` | root; `cetra-server` | `BatchLogProcessor`, `BatchConfigBuilder`, `SdkLoggerProvider`, `LogExporter`, `Resource`. Not `rt-tokio`, no experimental async processor, no `internal-logs`, no `metrics` |
| `opentelemetry-appender-tracing` | 0.33.1 (**new** in the graph; its dependencies are already in the lock) | `default-features = false` (its default set is empty) | root; `cetra-server` | `OpenTelemetryTracingBridge`: `tracing` events to log records |
| `opentelemetry-proto` | 0.33.1, unchanged | already has `gen-tonic-messages`, `logs`, `trace`; `transform::logs::tonic` is compiled | unchanged | `group_logs_by_resource_and_scope`, `ExportLogsServiceRequest` |
| `tracing-subscriber` | 0.3, locked at 0.3.23, unchanged pin | unchanged (`env-filter`, `json`, `fmt` and defaults) | moves from `cetra-server`'s `[dev-dependencies]` to `[dependencies]`; **removed** from `cetra-cli`'s `[dependencies]` | 0.3.23 has per-layer filters (`Layer::with_filter`), `Layer for Vec<L>` and `Box<dyn Layer>`, `FilterExt::and`, `filter_fn`, `SubscriberInitExt::try_init` |
| `flate2` | 1.1.11, unchanged | unchanged | **added** to `cetra-cli`'s `[dev-dependencies]` (TL1.6) | gunzip in the CLI's start-up-error export test |

All are Apache-2.0 or MIT, on the `deny.toml` allow-list. The opentelemetry family stays at one version (0.33.1). `deny.toml` is not edited. `tracing-opentelemetry` is M2's.

## Phase graph

```
TL1.1 dependencies ──┐
TL1.2 config ────────┼─► TL1.4 OTLP exporter ─► TL1.5 subscriber ─► TL1.6 CLI wiring
TL1.3 panic hook ────┘
TL1.7 docs and specs   (after TL1.2)
```

The coordinator schedules the phases across M1 and M4. Each phase touches these files:

| Phase | Files |
|---|---|
| TL1.1 | `Cargo.toml`, `crates/server/Cargo.toml`, `Cargo.lock` |
| TL1.2 | `crates/server/src/config.rs`, `crates/server/src/test_support/mod.rs`, `crates/server/src/ingest/fixtures_tests.rs`, `website/src/content/docs/docs/self-hosting/configuration-reference.md` |
| TL1.3 | `crates/server/src/lib.rs`, `crates/server/src/observability/mod.rs` (new), `crates/server/src/observability/panic.rs` (new) |
| TL1.4 | `crates/server/src/observability/otlp.rs` (new), `crates/server/src/observability/mod.rs` |
| TL1.5 | `crates/server/src/observability/subscriber.rs` (new), `crates/server/src/observability/mod.rs` |
| TL1.6 | `crates/cli/src/main.rs`, `crates/cli/Cargo.toml`, `Cargo.lock` |
| TL1.7 | `website/src/content/docs/docs/self-hosting/monitoring.md`, `website/src/content/docs/docs/self-hosting/backup.md`, `website/src/content/docs/docs/self-hosting/configuration.md`, `CHANGELOG.md`, `docs/spec/12-operations.md` |

Never two agents on the same file at once: `git commit -- <file>` commits the whole working-tree file, another agent's half-done edit included.

## Rules for every phase (the implementer is Haiku)

- Follow the plan exactly. Do not guess. If a step is unclear, contradicts the code, or cannot be done as written, stop and report the question instead of improvising.
- Edit only the files of the phase's **Edit** list. Report any change needed elsewhere.
- **Do not run** `cargo build`, `cargo test`, `cargo clippy`, `cargo check`, or any `just` recipe: the machine is loaded, and the coordinator runs the checks once at the end of the milestone. These commands are fine:
  - read-only commands: `cargo info`, `grep`, reading sources under `~/.cargo/registry/src`;
  - `rustfmt --edition 2024 <file>`, which formats without building. Run it only on leaf files you created or changed: `config.rs` and `ingest/fixtures_tests.rs` (TL1.2), `panic.rs`, `otlp.rs`, `subscriber.rs`. **Never** run it on a file that declares out-of-line modules: `lib.rs`, `observability/mod.rs`, `test_support/mod.rs`, `crates/cli/src/main.rs`. rustfmt recurses into the modules those files declare, which includes other agents' files. The coordinator's `just fmt` covers them;
  - where a phase says so, `cargo metadata --format-version 1 --offline > /dev/null`, which only rewrites `Cargo.lock` and compiles nothing.
- Write the tests the phase lists, even though you do not run them. Make them compile by reading the APIs: never guess a signature. Look it up in the registry sources named in **Read first**.
- **Done when** for every phase means: the listed files are changed as specified, nothing else changed (`git status`), and the commit exists with the given subject.
- Commit only your phase's files: `git commit -m "<subject>" -- <file> <file> …`. Other agents work in the same checkout. If `.git/index.lock` exists, wait a few seconds and retry. For a new file, run `git add <file>` first.
- Logs and error messages never contain a header value, a resource value, a URL's user info or path, a payload or a panic message (AGENTS.md § Privacy). They name keys, hosts, counts and static reasons.
- No `unwrap()` or `expect()` outside tests, unless a comment beside it states the invariant. No `unsafe`, which is forbidden workspace-wide: tests never call `std::env::set_var`.
- `mod` lines in `observability/mod.rs` stay in alphabetical order (rustfmt's `reorder_modules`), with M4's lines among them.
- The milestone's final check, run by the coordinator after TL1.7: `just fmt`, then `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check`, then `just deny`.

---

### TL1.1 — Dependencies

**Goal.** Declare the three OpenTelemetry crates in the workspace and in `cetra-server`, and make `tracing-subscriber` a normal dependency of `cetra-server`.

**Edit:**
- `Cargo.toml` (root)
- `crates/server/Cargo.toml`
- `Cargo.lock` (rewritten by `cargo metadata`)

**Steps.**
1. In the root `Cargo.toml` `[workspace.dependencies]`, right after the `opentelemetry-proto = { … }` entry (and its closing `] }`), add:
   ```toml
   # cetra's own log export (track TL, D-TL-2). The API crate, for `KeyValue`. No default features:
   # `internal-logs` would send the SDK's own diagnostics through `tracing`.
   opentelemetry = { version = "0.33.1", default-features = false, features = ["logs"] }
   # Batch log processor, logger provider and resource (D-TL-2). Never `rt-tokio` or an
   # `experimental_*` async processor: the batch processor runs on its own thread.
   opentelemetry_sdk = { version = "0.33.1", default-features = false, features = ["logs"] }
   # `tracing` events to OTel log records (D-TL-2).
   opentelemetry-appender-tracing = { version = "0.33.1", default-features = false }
   ```
2. In `crates/server/Cargo.toml` `[dependencies]`, after `opentelemetry-proto.workspace = true`, add:
   ```toml
   opentelemetry.workspace = true
   opentelemetry_sdk.workspace = true
   opentelemetry-appender-tracing.workspace = true
   tracing-subscriber.workspace = true
   ```
3. In `crates/server/Cargo.toml` `[dev-dependencies]`, delete the line `tracing-subscriber.workspace = true`, because it is now a normal dependency.
4. Run `cargo metadata --format-version 1 --offline > /dev/null` from the worktree root.
   - This updates `Cargo.lock` without compiling.
   - Check with `git diff Cargo.lock` that the only new `[[package]]` is `opentelemetry-appender-tracing` 0.33.1, and that `cetra-server`'s dependency list gained the four crates.
   - If any other package changed version, stop and report.
   - If `--offline` fails because the crate is not cached, run it without `--offline` and report that.

**Tests.** None. A dependency declaration has no behaviour; the final `just deny` checks licences and duplicates.

**Done when.** The three files are changed as above and committed.

**Commit:** `Add the OpenTelemetry SDK and log appender dependencies`

---

### TL1.2 — The `[self_monitoring]` configuration

**Goal.** `ServerConfig` gains `self_monitoring` with M1's five keys, validated without ever quoting a header or resource value, and the configuration reference lists them.

**Edit:**
- `crates/server/src/config.rs`
- `crates/server/src/test_support/mod.rs` (two `ServerConfig` literals and the `use crate::config::{…}` list only)
- `crates/server/src/ingest/fixtures_tests.rs` (one `ServerConfig` literal and the `use crate::config::{…}` list only)
- `website/src/content/docs/docs/self-hosting/configuration-reference.md`

**Steps.**
1. In `config.rs`, after `impl Default for LogConfig`, add these items verbatim (docs included):
   ```rust
   /// `self_monitoring.otlp_endpoint` (track TL, D-TL-1): an `http` or `https` base URL without
   /// user info, query or fragment. Its path is kept and `/v1/logs` is appended to it.
   #[derive(Clone, PartialEq, Eq)]
   pub struct OtlpEndpoint {
       /// The URL without a trailing `/`, for example `https://otlp.example.com/otlp`.
       base: String,
       /// The host as written in the URL (IPv6 in brackets), for logs.
       host: String,
   }

   impl OtlpEndpoint {
       /// The URL log records are posted to: the base followed by `/v1/logs`.
       pub fn logs_url(&self) -> String {
           format!("{}/v1/logs", self.base)
       }

       /// The endpoint's host, the only part of it cetra logs.
       pub fn host(&self) -> &str {
           &self.host
       }
   }

   /// Prints the host only: some vendors put a token in the path.
   impl fmt::Debug for OtlpEndpoint {
       fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
           formatter
               .debug_struct("OtlpEndpoint")
               .field("host", &self.host)
               .finish_non_exhaustive()
       }
   }

   /// `[self_monitoring]`: export of cetra's own logs to the operator's OTLP/HTTP endpoint
   /// (track TL, D-TL-1, D-TL-3). Off while `otlp_endpoint` is unset.
   #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
   #[serde(deny_unknown_fields)]
   pub struct SelfMonitoringConfig {
       /// Base URL of the OTLP/HTTP endpoint. Unset or blank: no export.
       #[serde(default, deserialize_with = "deserialize_otlp_endpoint")]
       pub otlp_endpoint: Option<OtlpEndpoint>,
       /// Request headers, `name=value,name=value` (the `OTEL_EXPORTER_OTLP_HEADERS` syntax).
       #[serde(default)]
       pub otlp_headers: Option<Secret<String>>,
       /// Whether log records are exported. Defaults to `true`.
       #[serde(default = "default_self_monitoring_logs")]
       pub logs: bool,
       /// `EnvFilter` directive of the exported events. Defaults to `warn`. The subscriber
       /// checks it and caps `rmcp` at `warn`, as for stdout.
       #[serde(default = "default_self_monitoring_level")]
       pub level: String,
       /// Extra resource attributes, `key=value,key=value` (the `OTEL_RESOURCE_ATTRIBUTES` syntax).
       #[serde(default)]
       pub resource: String,
   }

   impl Default for SelfMonitoringConfig {
       fn default() -> Self {
           Self {
               otlp_endpoint: None,
               otlp_headers: None,
               logs: default_self_monitoring_logs(),
               level: default_self_monitoring_level(),
               resource: String::new(),
           }
       }
   }

   impl SelfMonitoringConfig {
       /// The endpoint, when log records are exported: `otlp_endpoint` is set and `logs` is true.
       pub fn logs_endpoint(&self) -> Option<&OtlpEndpoint> {
           self.otlp_endpoint.as_ref().filter(|_| self.logs)
       }

       /// Whether anything is exported. M1 exports logs only; M2 adds traces here.
       pub fn exports(&self) -> bool {
           self.logs_endpoint().is_some()
       }
   }
   ```
2. In `ServerConfig`, after the `log` field, add:
   ```rust
       /// `[self_monitoring]`: export of cetra's own logs over OTLP/HTTP (track TL, D-TL-1).
       #[serde(default)]
       pub self_monitoring: SelfMonitoringConfig,
   ```
3. After `fn default_log_level`, add:
   ```rust
   fn default_self_monitoring_logs() -> bool {
       true
   }

   fn default_self_monitoring_level() -> String {
       "warn".to_owned()
   }
   ```
4. After `fn parse_public_url`, add the endpoint parser, the pair parsers and their constants, verbatim:
   ```rust
   /// Most entries in `self_monitoring.otlp_headers` or `self_monitoring.resource`.
   pub(crate) const MAX_PAIRS: usize = 32;

   /// Longest `self_monitoring.otlp_headers` or `self_monitoring.resource` value, in bytes.
   pub(crate) const MAX_PAIRS_BYTES: usize = 8192;

   /// Headers cetra sets itself on every export request.
   const RESERVED_HEADERS: &[&str] = &[
       "content-type",
       "content-encoding",
       "content-length",
       "host",
       "user-agent",
   ];

   /// Why a reserved header is refused.
   const RESERVED_HEADER_REASON: &str =
       "content-type, content-encoding, content-length, host and user-agent are set by cetra";

   /// Checks `self_monitoring.otlp_endpoint`. The reasons never quote the input.
   pub(crate) fn parse_otlp_endpoint(raw: &str) -> Result<OtlpEndpoint, &'static str> {
       let raw = raw.trim();
       if !is_http_url(raw) {
           return Err("must be an http or https URL");
       }
       let url = url::Url::parse(raw).map_err(|_| "must be an http or https URL")?;
       if !url.username().is_empty() || url.password().is_some() {
           return Err("must not contain user info");
       }
       if url.query().is_some() || url.fragment().is_some() {
           return Err("must not have a query or fragment");
       }
       let path = url.path().trim_end_matches('/');
       if path.ends_with("/v1/logs") || path.ends_with("/v1/traces") {
           return Err("must be the base URL, without /v1/logs or /v1/traces");
       }
       let Some(host) = url.host_str() else {
           return Err("must have a host");
       };
       Ok(OtlpEndpoint {
           host: host.to_owned(),
           // No query or fragment, so the serialization ends with the path.
           base: url.as_str().trim_end_matches('/').to_owned(),
       })
   }

   /// `name=value` entries separated by `,` (the `OTEL_EXPORTER_OTLP_HEADERS` and
   /// `OTEL_RESOURCE_ATTRIBUTES` syntax). Each entry is split at its first `=`. Names and values
   /// are trimmed, and values are percent-decoded (`%20` is a space; `+` stays `+`, as in
   /// base64). Empty entries are skipped. The reasons never quote the input.
   fn parse_pairs(raw: &str) -> Result<Vec<(String, String)>, &'static str> {
       if raw.len() > MAX_PAIRS_BYTES {
           return Err("at most 8192 bytes");
       }
       let mut pairs = Vec::new();
       for entry in raw.split(',') {
           let entry = entry.trim();
           if entry.is_empty() {
               continue;
           }
           let Some((name, value)) = entry.split_once('=') else {
               return Err("entries must be name=value");
           };
           let name = name.trim();
           if name.is_empty() {
               return Err("a name is empty");
           }
           let value =
               percent_decode(value.trim()).ok_or("a value is not valid percent-encoding")?;
           pairs.push((name.to_owned(), value));
       }
       if pairs.len() > MAX_PAIRS {
           return Err("at most 32 entries");
       }
       Ok(pairs)
   }

   /// `%XX` sequences decoded; any other byte kept. `None` for a bad sequence or a result that
   /// is not UTF-8.
   fn percent_decode(raw: &str) -> Option<String> {
       let bytes = raw.as_bytes();
       let mut decoded = Vec::with_capacity(bytes.len());
       let mut index = 0;
       while let Some(&byte) = bytes.get(index) {
           if byte == b'%' {
               let high = bytes.get(index + 1).and_then(|b| char::from(*b).to_digit(16))?;
               let low = bytes.get(index + 2).and_then(|b| char::from(*b).to_digit(16))?;
               decoded.push(u8::try_from(high * 16 + low).ok()?);
               index += 3;
           } else {
               decoded.push(byte);
               index += 1;
           }
       }
       String::from_utf8(decoded).ok()
   }

   /// `self_monitoring.otlp_headers` as `(lower-case name, value)` pairs, in order.
   pub(crate) fn parse_otlp_headers(raw: &str) -> Result<Vec<(String, String)>, &'static str> {
       let mut headers = parse_pairs(raw)?;
       let mut seen = BTreeSet::new();
       for (name, value) in &mut headers {
           if !name.bytes().all(is_token_byte) {
               return Err("a header name is not a valid HTTP token");
           }
           name.make_ascii_lowercase();
           if RESERVED_HEADERS.contains(&name.as_str()) {
               return Err(RESERVED_HEADER_REASON);
           }
           if !seen.insert(name.clone()) {
               return Err("a name is listed twice");
           }
           if !value
               .bytes()
               .all(|byte| byte == b'\t' || (b' '..=b'~').contains(&byte))
           {
               return Err("a header value has a character HTTP does not allow");
           }
       }
       Ok(headers)
   }

   /// RFC 9110 `tchar`.
   fn is_token_byte(byte: u8) -> bool {
       byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
   }

   /// `self_monitoring.resource` as `(key, value)` pairs, in order.
   pub(crate) fn parse_resource(raw: &str) -> Result<Vec<(String, String)>, &'static str> {
       let pairs = parse_pairs(raw)?;
       let mut seen = BTreeSet::new();
       for (key, _) in &pairs {
           if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
               return Err("a key has whitespace or a control character");
           }
           if !seen.insert(key.as_str()) {
               return Err("a name is listed twice");
           }
       }
       Ok(pairs)
   }

   fn validate_self_monitoring(monitoring: &SelfMonitoringConfig) -> Result<(), ConfigError> {
       if let Some(headers) = &monitoring.otlp_headers {
           parse_otlp_headers(headers.expose()).map_err(|reason| ConfigError::Invalid {
               key: "self_monitoring.otlp_headers",
               reason,
           })?;
       }
       parse_resource(&monitoring.resource).map_err(|reason| ConfigError::Invalid {
           key: "self_monitoring.resource",
           reason,
       })?;
       Ok(())
   }
   ```
5. After `fn deserialize_optional_socket_addr`, add:
   ```rust
   /// A blank string is `None` (no export), so `CETRA_SELF_MONITORING__OTLP_ENDPOINT=` unsets it.
   fn deserialize_otlp_endpoint<'de, D: Deserializer<'de>>(
       deserializer: D,
   ) -> Result<Option<OtlpEndpoint>, D::Error> {
       let Some(raw) = Option::<String>::deserialize(deserializer)? else {
           return Ok(None);
       };
       if raw.trim().is_empty() {
           return Ok(None);
       }
       parse_otlp_endpoint(&raw).map(Some).map_err(de::Error::custom)
   }
   ```
6. In `validate`, call `validate_self_monitoring(&config.self_monitoring)?;` right after `validate_init(&config.init)?;`.
7. In `secret_key`, before the `[..] if path.last()…client_secret` arm, add:
   ```rust
           [parent, key] if parent == "self_monitoring" && key == "otlp_headers" => {
               Some("self_monitoring.otlp_headers")
           }
   ```
8. In `is_secret_path`, add `| "self_monitoring.otlp_headers"` to the `matches!` list. Its env value then stays a string: an all-digit value is never turned into a number and printed.
9. Add `self_monitoring: SelfMonitoringConfig::default(),` right after the `log: LogConfig::default(),` line of each full `ServerConfig { … }` literal, and add `SelfMonitoringConfig` to that file's `use crate::config::{…}` list (alphabetical). Without it, these literals no longer compile. There are three:
   - `crates/server/src/test_support/mod.rs`: `fn default_config` (line ~1258) and `pub(crate) fn config` (line ~1815);
   - `crates/server/src/ingest/fixtures_tests.rs`: `fn serve_config` (line ~438).
   Search both files for `ServerConfig {` and change nothing else. M4's TL4.3 later adds only `telemetry:` to the same literals.
10. Change `fn load_from` to `pub(crate) fn load_from`, so the observability tests (TL1.5) build a `ServerConfig` from TOML. It has no doc comment today. Add `/// Defaults, then \`toml\`, then \`env\`, validated. Also used by tests outside this module to build a config from TOML.`
11. Do **not** touch `ALIASES` or `is_filtered`. The keys have only the split env form (`CETRA_SELF_MONITORING__OTLP_ENDPOINT`, …), which `split_key` already maps.
12. In `configuration-reference.md`:
    - In the `restore_mode` row, append ` The self-monitoring export still runs: it goes to your own endpoint.` to the Description cell.
    - At the end of the page, after the `[log]` table, add:
    ```markdown

    ## `[self_monitoring]`

    Export cetra's own logs to an OTLP/HTTP endpoint you choose (Sentry, Grafana Cloud, an OpenTelemetry Collector). Off while `otlp_endpoint` is unset. See [Monitoring](/docs/self-hosting/monitoring/#export-logs-over-otlp).

    | Key | Environment | Type | Default | Description |
    |---|---|---|---|---|
    | `self_monitoring.otlp_endpoint` | | URL | unset (off) | Base URL of the OTLP/HTTP endpoint, `http` or `https`, without user info, query or fragment. cetra posts protobuf, gzip-compressed, to the URL followed by `/v1/logs`; a URL that already ends in `/v1/logs` or `/v1/traces` is refused. |
    | `self_monitoring.otlp_headers` | | secret | unset | Request headers, `name=value` pairs separated by commas, as in `OTEL_EXPORTER_OTLP_HEADERS`. Values may be percent-encoded (`Basic%20…`). At most 32 headers and 8192 bytes; `content-type`, `content-encoding`, `content-length`, `host` and `user-agent` are set by cetra. |
    | `self_monitoring.logs` | | boolean | `true` | Export log records. |
    | `self_monitoring.level` | | string | `"warn"` | A log filter for the exported events, like `log.level`. `rmcp` is capped at `warn`. |
    | `self_monitoring.resource` | | string | `""` | Extra resource attributes, `key=value` pairs separated by commas, as in `OTEL_RESOURCE_ATTRIBUTES`. Values may be percent-encoded (`%2C` for a comma). At most 32 attributes and 8192 bytes. They are added after cetra's own (`service.name`, `service.version`, `service.instance.id`, `host.arch`, `os.type`) and replace one of the same key. |
    ```

**Tests** (in `config.rs` `mod tests`; add `parse_otlp_endpoint`, `parse_otlp_headers`, `parse_resource`, `SelfMonitoringConfig` to its `use super::{…}`):
- `self_monitoring_defaults`: `load_from(None, &[])` gives:
  - `otlp_endpoint` and `otlp_headers` `None`, `logs` true, `level` `"warn"`, `resource` `""`;
  - `logs_endpoint()` `None` and `exports()` false.
  Also add the line `assert_eq!(config.self_monitoring, SelfMonitoringConfig::default());` to `defaults_match_spec`.
- `self_monitoring_loads_from_toml_and_env`:
  - TOML `[self_monitoring]\notlp_endpoint = "https://otlp.example.com/otlp/"\n`;
  - env `CETRA_SELF_MONITORING__OTLP_HEADERS=authorization=Basic%20abc`, `CETRA_SELF_MONITORING__LOGS=false`, `CETRA_SELF_MONITORING__LEVEL=info`, `CETRA_SELF_MONITORING__RESOURCE=deployment.environment=prod,cetra.cell=eu1`.
  - Expect `logs_url()` `https://otlp.example.com/otlp/v1/logs`, `host()` `otlp.example.com`, `logs` false, `logs_endpoint()` `None`, `exports()` false, `level` `info`, `resource` unchanged.
  - The same TOML without the env gives `exports()` true.
  - The headers' `Debug` prints `Secret(***)` and not `abc`.
- `otlp_endpoint_rules`: `parse_otlp_endpoint` on each input gives exactly the `logs_url()` or the reason:
  - `https://o1.ingest.de.sentry.io/api/2/integration/otlp` → `…/integration/otlp/v1/logs`
  - `http://collector:4318` and `http://collector:4318/` → `http://collector:4318/v1/logs`
  - `https://otlp-gateway-prod-eu-west-2.grafana.net/otlp//` → `https://otlp-gateway-prod-eu-west-2.grafana.net/otlp/v1/logs`
  - `http://[::1]:4318` → `http://[::1]:4318/v1/logs`, with host `[::1]`
  - `https://x/api/s3cr3t/otlp`: `format!("{endpoint:?}")` contains `x` and not `s3cr3t` (`Debug` prints the host only)
  - `ftp://x` and `not a url` and `https:///x` → `must be an http or https URL`
  - `https://user:pw@x` → `must not contain user info`
  - `https://x/otlp?token=1` and `https://x/otlp#f` → `must not have a query or fragment`
  - `https://x/otlp/v1/logs` and `https://x/v1/traces/` → `must be the base URL, without /v1/logs or /v1/traces`
- `a_blank_otlp_endpoint_is_unset`: env `CETRA_SELF_MONITORING__OTLP_ENDPOINT=` and `=   ` give `None`.
- `an_invalid_otlp_endpoint_names_the_rule_not_the_value`: TOML `otlp_endpoint = "https://user:hunter2@x"` fails. The error text contains `must not contain user info` and `otlp_endpoint`, and does not contain `hunter2`.
- `otlp_headers_rules`: `parse_otlp_headers` gives:
  - `authorization=Basic%20abc` → `[("authorization", "Basic abc")]`
  - `x-sentry-auth=sentry sentry_key=abc` → `[("x-sentry-auth", "sentry sentry_key=abc")]`
  - ` A = b , ,c=d ` → `[("a", "b"), ("c", "d")]`
  - `a=` → `[("a", "")]`
  - `` (empty) → `[]`
  - `k=YWJj+/9=` → `[("k", "YWJj+/9=")]` (base64 `+` kept)
  - `novalue` → `entries must be name=value`
  - `=v` → `a name is empty`
  - `bad name=v` → `a header name is not a valid HTTP token`
  - `a=%zz` and `a=%4` and `a=%FF` → `a value is not valid percent-encoding`
  - `a=%0A` → `a header value has a character HTTP does not allow`
  - `a=1,A=2` → `a name is listed twice`
  - `Content-Type=x` and `user-agent=x` → the reserved-header reason
  - 33 entries `h0=v,…,h32=v` → `at most 32 entries`
  - a string of 8193 bytes → `at most 8192 bytes`
- `resource_rules`: `parse_resource` gives:
  - `deployment.environment=prod,cetra.cell=eu1` → the two pairs in order
  - `k=v%2Cw` → `[("k", "v,w")]`
  - `Service.Name=x` keeps the case
  - `k v=1` → `a key has whitespace or a control character`
  - `=1` → `a name is empty`
  - `k=1,k=2` → `a name is listed twice`
  - `k=%FF` → `a value is not valid percent-encoding`
- `invalid_otlp_headers_never_reach_the_error`:
  - env `CETRA_SELF_MONITORING__OTLP_HEADERS=12345` fails with `ConfigError::Invalid { key: "self_monitoring.otlp_headers", reason: "entries must be name=value" }`, and the error's `Display` does not contain `12345`;
  - TOML `otlp_headers = 5` fails with `ConfigError::Invalid { key: "self_monitoring.otlp_headers", reason: "expected a string" }`.
- `invalid_resource_is_refused`: env `CETRA_SELF_MONITORING__RESOURCE=nokey` fails with `Invalid { key: "self_monitoring.resource", reason: "entries must be name=value" }`.
- `unknown_self_monitoring_key_is_refused`: TOML `[self_monitoring]\ntraces = true\n` fails, because `traces` is M2's key.
- `the_configuration_reference_matches_the_loader` (existing) must pass with the five new rows: the implementer checks by reading that each new key is a field and each field a row.

**Done when.** The four files are changed as above and committed.

**Commit:** `Add the self_monitoring configuration`

---

### TL1.3 — The observability module and the panic hook

**Goal.** Create `observability` with a panic hook that chains the default hook and emits an ERROR `panic` event with the location only.

**Edit:**
- `crates/server/src/lib.rs`
- `crates/server/src/observability/mod.rs` (new)
- `crates/server/src/observability/panic.rs` (new)

**Steps.**
1. In `lib.rs`:
   - Add `pub mod observability;` in alphabetical order (after `pub(crate) mod mcp;` and `pub(crate) mod meta;`, before `pub(crate) mod prices;`).
   - Extend the crate doc with one line: `` //! [`observability`] holds cetra's own log export, its subscriber and its panic hook (track TL). ``
2. Create `observability/mod.rs`:
   ```rust
   //! cetra's own observability (track TL, `docs/plans/tl-telemetry/README.md`): the panic hook,
   //! the OTLP log export to the operator's endpoint, and the `tracing` subscriber the CLI
   //! installs. Prometheus `/metrics` is [`crate::telemetry`] (D-N-28).

   pub mod panic;
   ```
   (TL1.4 and TL1.5, and M4, add their `mod` lines in alphabetical order with this one.)
3. Create `observability/panic.rs` with exactly this content (doc comments included):
   ```rust
   //! The panic hook of `cetra serve` (track TL, design § Panics).
   //!
   //! It chains Rust's default hook, so stderr keeps the message as before, then emits one ERROR
   //! event `panic` with the code location. The message is never recorded: its text can hold data
   //! (`unwrap()` prints the error's `Debug`; a bad string slice prints the string). Task panics
   //! that tokio catches go through the same hook.

   use std::cell::Cell;
   use std::panic::PanicHookInfo;
   use std::sync::Once;

   /// The target of the `panic` event, this module's path. Track TL M5's crash layer skips events
   /// with it, so a panic is reported once.
   pub(crate) const PANIC_EVENT_TARGET: &str = "cetra_server::observability::panic";

   thread_local! {
       /// Set while this thread runs the hook: a panic inside the hook (a layer that panics)
       /// skips the event instead of recursing.
       static IN_HOOK: Cell<bool> = const { Cell::new(false) };
   }

   /// Installs the hook. Only the first call in a process does anything, so the default hook is
   /// chained once. `cetra serve` calls it before it loads the config.
   pub fn install() {
       static INSTALLED: Once = Once::new();
       INSTALLED.call_once(|| {
           let default = std::panic::take_hook();
           std::panic::set_hook(Box::new(move |info| {
               default(info);
               report(info);
           }));
       });
   }

   /// What the hook adds to the default hook. Track TL M5 adds the crash-report spool here, after
   /// the event.
   fn report(info: &PanicHookInfo<'_>) {
       let Some(_guard) = HookGuard::enter() else {
           return;
       };
       let location = panic_location(info);
       tracing::error!(target: PANIC_EVENT_TARGET, location = %location, "panic");
   }

   /// `file:line:column` of the panic, or `unknown`.
   pub(crate) fn panic_location(info: &PanicHookInfo<'_>) -> String {
       info.location().map_or_else(
           || "unknown".to_owned(),
           |location| {
               format!(
                   "{}:{}:{}",
                   location.file(),
                   location.line(),
                   location.column()
               )
           },
       )
   }

   /// Marks this thread as inside the hook until dropped.
   struct HookGuard;

   impl HookGuard {
       /// `None` when this thread is already inside the hook, or when its thread-local storage is
       /// gone (a panic during thread exit).
       fn enter() -> Option<Self> {
           let entered = IN_HOOK
               .try_with(|flag| !flag.replace(true))
               .unwrap_or(false);
           entered.then_some(Self)
       }
   }

   impl Drop for HookGuard {
       fn drop(&mut self) {
           let _ = IN_HOOK.try_with(|flag| flag.set(false));
       }
   }
   ```

**Tests** (in `panic.rs`, `#[cfg(test)] mod tests`):
- `the_panic_target_is_this_module`: `assert_eq!(PANIC_EVENT_TARGET, module_path!().trim_end_matches("::tests"))`. Inside `mod tests`, `module_path!()` ends with `::tests`.
- `hook_guard_refuses_a_second_entry`: `let first = HookGuard::enter();` is `Some`. `HookGuard::enter()` is then `None`. After `drop(first)`, `enter()` is `Some` again.
- `a_panic_emits_an_error_event_with_the_location_only`:
  1. Call `crate::test_support::ensure_global_subscriber()`, then `install()` twice: the second call must do nothing.
  2. Build a capturing dispatch like `test_support::capture_logs` (a `fmt::layer()` with `.with_ansi(false).with_target(true)` writing to an `Arc<Mutex<Vec<u8>>>`) and set it with `tracing::dispatcher::set_default`.
  3. Run `std::panic::catch_unwind(|| panic!("sk-cetra-planted0000000000000000000000000 user@example.com prompt text"))` and assert it is `Err`.
  4. Drop the guard and read the buffer.
  5. Assert that it contains `ERROR`, `cetra_server::observability::panic`, `panic` and `location=`, and `observability/panic.rs`. It contains no `sk-cetra-planted`, no `user@example.com` and no `prompt text`. It holds **exactly one** line with `location=`, which proves `install` chained once.
  - The default hook also prints the message on stderr. That is expected test noise.
  - `test_support`'s `LogBuffer` is private: write a local writer with the shape of the CLI test's `Buffer` (`struct Buffer(Arc<Mutex<Vec<u8>>>)` implementing `std::io::Write`).

**Done when.** The three files are created or changed as above and committed.

**Commit:** `Add the self-monitoring panic hook`

---

### TL1.4 — The OTLP/HTTP log exporter

**Goal.** `LogPipeline` builds an env-free `SdkLoggerProvider` around cetra's `OtlpLogExporter`, which posts gzip protobuf batches to `<endpoint>/v1/logs` through the tokio runtime and logs failures at most once a minute.

**Edit:**
- `crates/server/src/observability/otlp.rs` (new)
- `crates/server/src/observability/mod.rs`

**Steps.**
1. In `observability/mod.rs`:
   - Add `pub(crate) mod otlp;` before `pub mod panic;` (alphabetical order).
   - At the end, add:
   ```rust
   /// Why the subscriber or the export could not be built. Messages name keys and static
   /// reasons, never a header or resource value.
   #[derive(Debug, thiserror::Error)]
   pub enum ObservabilityError {
       /// The export's HTTP client could not be built.
       #[error("self_monitoring: cannot build the HTTP client")]
       Client,
       /// `self_monitoring.otlp_headers` did not convert to HTTP headers.
       #[error("self_monitoring.otlp_headers: invalid header")]
       Header,
       /// `self_monitoring.resource` did not parse.
       #[error("self_monitoring.resource: invalid attribute list")]
       Resource,
   }
   ```
2. Create `observability/otlp.rs`.
   - Module doc: `//! cetra's OTLP/HTTP log exporter for self-monitoring (track TL, D-TL-2): opentelemetry-proto's transform, prost, gzip and reqwest 0.12. Not opentelemetry-otlp, which merges the host's OTEL_EXPORTER_OTLP_* variables into the request. The SDK's batch processor runs on its own thread and calls [`OtlpLogExporter::export`] through futures_executor::block_on, so the request itself runs on the tokio runtime.`
   - Imports to use (verify each path in the registry sources):
     - `opentelemetry_sdk::logs::{BatchConfig, BatchConfigBuilder, BatchLogProcessor, LogBatch, LogExporter, SdkLogger, SdkLoggerProvider}`
     - `opentelemetry_sdk::Resource`
     - `opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult}`
     - `opentelemetry::KeyValue`
     - `opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge`
     - `opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema`
     - `opentelemetry_proto::transform::logs::tonic::group_logs_by_resource_and_scope`
     - `opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest`
     - `prost::Message`
     - `flate2::{Compression, write::GzEncoder}`
     - `reqwest::header::{CONTENT_ENCODING, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT}`
     - `tokio::runtime::Handle`
     - `std::fmt`, `std::io::Write as _`, `std::sync::{Arc, Mutex, PoisonError}`, `std::time::{Duration, Instant}`
     - `crate::config::{OtlpEndpoint, Secret, SelfMonitoringConfig, parse_otlp_headers, parse_resource}`
     - `super::ObservabilityError`
3. Add these constants, verbatim:
   ```rust
   /// The target of the export's own failure lines. The export filter drops it, so a failing
   /// export is never exported (design § Feedback loops).
   pub(crate) const EXPORT_FAILURE_TARGET: &str = "cetra_self_monitoring";

   /// Timeout of one export request, connection included.
   const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

   /// At most one failure line per interval.
   const FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(60);

   /// Records buffered before new ones are dropped.
   const MAX_QUEUE_SIZE: usize = 2_048;

   /// Delay between two exports.
   const SCHEDULED_DELAY: Duration = Duration::from_secs(1);

   /// Records per request.
   const MAX_EXPORT_BATCH_SIZE: usize = 512;
   ```
4. Add `batch_config`, verbatim:
   ```rust
   /// Every setting given: `BatchConfigBuilder::default()` reads `OTEL_BLRP_*`, and a value set
   /// here replaces the one it read. (`OTEL_BLRP_EXPORT_TIMEOUT` is read only behind an SDK
   /// feature cetra does not enable; [`REQUEST_TIMEOUT`] bounds an export.)
   fn batch_config() -> BatchConfig {
       BatchConfigBuilder::default()
           .with_max_queue_size(MAX_QUEUE_SIZE)
           .with_scheduled_delay(SCHEDULED_DELAY)
           .with_max_export_batch_size(MAX_EXPORT_BATCH_SIZE)
           .build()
   }
   ```
5. Add the resource, verbatim:
   ```rust
   /// cetra's resource attributes, then the operator's (which win on the same key). Built from
   /// `Resource::builder_empty()`: `Resource::builder()` reads `OTEL_RESOURCE_ATTRIBUTES` and
   /// `OTEL_SERVICE_NAME`.
   fn resource(extra: Vec<(String, String)>) -> Resource {
       Resource::builder_empty()
           .with_attributes([
               KeyValue::new("service.name", "cetra"),
               KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
               KeyValue::new("service.instance.id", uuid::Uuid::now_v7().to_string()),
               KeyValue::new("host.arch", host_arch()),
               KeyValue::new("os.type", os_type()),
           ])
           .with_attributes(extra.into_iter().map(|(key, value)| KeyValue::new(key, value)))
           .build()
   }

   /// `host.arch` in the OTel semantic-convention spelling.
   fn host_arch() -> &'static str {
       match std::env::consts::ARCH {
           "x86_64" => "amd64",
           "aarch64" => "arm64",
           "arm" => "arm32",
           "powerpc" => "ppc32",
           "powerpc64" => "ppc64",
           other => other,
       }
   }

   /// `os.type` in the OTel semantic-convention spelling.
   fn os_type() -> &'static str {
       match std::env::consts::OS {
           "macos" => "darwin",
           other => other,
       }
   }
   ```
6. Add `request_headers`, verbatim:
   ```rust
   /// The operator's headers (marked sensitive), then cetra's own. `config::parse_otlp_headers`
   /// already checked the names and values at load.
   fn request_headers(raw: Option<&Secret<String>>) -> Result<HeaderMap, ObservabilityError> {
       let mut headers = HeaderMap::new();
       if let Some(raw) = raw {
           let pairs = parse_otlp_headers(raw.expose()).map_err(|_| ObservabilityError::Header)?;
           for (name, value) in pairs {
               let name =
                   HeaderName::from_bytes(name.as_bytes()).map_err(|_| ObservabilityError::Header)?;
               let mut value = HeaderValue::from_str(&value).map_err(|_| ObservabilityError::Header)?;
               value.set_sensitive(true);
               headers.append(name, value);
           }
       }
       // `from_static` panics only on bytes outside visible ASCII; these literals are ASCII.
       headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/x-protobuf"));
       headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
       headers.insert(
           USER_AGENT,
           HeaderValue::from_static(concat!("cetra/", env!("CARGO_PKG_VERSION"))),
       );
       Ok(headers)
   }
   ```
7. Add the failure throttle, verbatim:
   ```rust
   /// Lets one failure line through per [`FAILURE_LOG_INTERVAL`].
   #[derive(Debug, Default)]
   struct FailureLog {
       last: Mutex<Option<Instant>>,
   }

   impl FailureLog {
       /// Whether a failure seen at `now` is logged. A poisoned lock is recovered: the value is
       /// only a time.
       fn should_log(&self, now: Instant) -> bool {
           let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
           match *last {
               Some(at) if now.saturating_duration_since(at) < FAILURE_LOG_INTERVAL => false,
               _ => {
                   *last = Some(now);
                   true
               }
           }
       }
   }

   /// Why an export failed. `reason` is one of `encode`, `status`, `timeout`, `connect`,
   /// `request`, `runtime`; never error text, which can quote the URL.
   #[derive(Debug, PartialEq, Eq)]
   struct Failure {
       reason: &'static str,
       status: Option<u16>,
   }

   impl Failure {
       fn new(reason: &'static str, status: Option<u16>) -> Self {
           Self { reason, status }
       }
   }

   /// The outcome of one request, as the runtime returned it.
   fn outcome(
       result: Result<Result<reqwest::StatusCode, reqwest::Error>, tokio::task::JoinError>,
   ) -> Result<(), Failure> {
       match result {
           Ok(Ok(status)) if status.is_success() => Ok(()),
           Ok(Ok(status)) => Err(Failure::new("status", Some(status.as_u16()))),
           Ok(Err(error)) if error.is_timeout() => Err(Failure::new("timeout", None)),
           Ok(Err(error)) if error.is_connect() => Err(Failure::new("connect", None)),
           Ok(Err(_)) => Err(Failure::new("request", None)),
           Err(_) => Err(Failure::new("runtime", None)),
       }
   }

   fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
       let buffer = Vec::with_capacity(bytes.len() / 4);
       let mut encoder = GzEncoder::new(buffer, Compression::default());
       encoder.write_all(bytes)?;
       encoder.finish()
   }

   /// No redirects, [`REQUEST_TIMEOUT`]. `HTTP(S)_PROXY` is honoured, like `identity_client`,
   /// except in this crate's tests, whose receivers listen on 127.0.0.1.
   fn http_client() -> Result<reqwest::Client, ObservabilityError> {
       let builder = reqwest::Client::builder()
           .redirect(reqwest::redirect::Policy::none())
           .timeout(REQUEST_TIMEOUT);
       #[cfg(test)]
       let builder = builder.no_proxy();
       builder.build().map_err(|_| ObservabilityError::Client)
   }
   ```
8. Add the exporter:
   ```rust
   /// Posts each batch to `<otlp_endpoint>/v1/logs` (D-TL-2).
   pub(crate) struct OtlpLogExporter {
       client: reqwest::Client,
       url: String,
       headers: HeaderMap,
       resource: ResourceAttributesWithSchema,
       runtime: Handle,
       failures: Arc<FailureLog>,
   }
   ```
   1. Write a manual `impl fmt::Debug for OtlpLogExporter`. It prints `url` and `header_names` (the `headers.keys()` names only, collected to a `Vec<&str>`), with `finish_non_exhaustive()`. It never prints a header value.
   2. Write `impl LogExporter for OtlpLogExporter`. `export` is a plain `fn`, not an `async fn`, so the `LogBatch` borrow ends before the returned future. The comments are part of the code. Write `export` exactly as:
      ```rust
      fn export(&self, batch: LogBatch<'_>) -> impl Future<Output = OTelSdkResult> + Send {
          let records = batch.iter().count();
          let request = ExportLogsServiceRequest {
              resource_logs: group_logs_by_resource_and_scope(&batch, &self.resource),
          };
          // `send()` runs inside the spawned task: reqwest creates its timeout timer there, which
          // needs the runtime's context, and this (the SDK's batch) thread has none.
          let sent = gzip(&request.encode_to_vec()).map(|body| {
              let builder = self
                  .client
                  .post(&self.url)
                  .headers(self.headers.clone())
                  .body(body);
              self.runtime
                  .spawn(async move { builder.send().await.map(|response| response.status()) })
          });
          let failures = Arc::clone(&self.failures);
          async move {
              let result = match sent {
                  Ok(handle) => outcome(handle.await),
                  Err(_) => Err(Failure::new("encode", None)),
              };
              let Err(failure) = result else {
                  return Ok(());
              };
              if failures.should_log(Instant::now()) {
                  tracing::warn!(
                      target: EXPORT_FAILURE_TARGET,
                      reason = failure.reason,
                      status = failure.status,
                      records,
                      "self-monitoring log export failed"
                  );
              }
              Err(OTelSdkError::InternalFailure(failure.reason.to_owned()))
          }
      }
      ```
      The response body is never read. `Future` is in the 2024 prelude: no import is needed.
   3. `fn set_resource(&mut self, resource: &Resource) { self.resource = resource.into(); }`
      - The processor forwards the provider's resource on its control channel. The same value is also given at construction (step 9), because a batch could be exported before that message is read.
   4. Leave `shutdown_with_timeout` and `event_enabled` at their defaults.
9. Add the pipeline:
   ```rust
   /// The provider of the self-monitoring log export. The bridge layer emits into it; the
   /// subscriber's guard shuts it down at exit.
   pub(crate) struct LogPipeline {
       provider: SdkLoggerProvider,
       host: String,
   }

   impl LogPipeline {
       /// Builds the exporter, its batch processor and the provider. No `OTEL_*` variable is read.
       pub(crate) fn new(
           config: &SelfMonitoringConfig,
           endpoint: &OtlpEndpoint,
           runtime: Handle,
       ) -> Result<Self, ObservabilityError>;

       /// The `tracing` layer that turns events into log records of this pipeline.
       pub(crate) fn bridge(&self) -> OpenTelemetryTracingBridge<SdkLoggerProvider, SdkLogger>;

       /// The endpoint's host, for the start-up line.
       pub(crate) fn host(&self) -> &str;

       /// Exports what is queued and stops, waiting at most `timeout`. Blocks the calling
       /// thread: call it from a blocking thread, never from an async task.
       pub(crate) fn shutdown(&self, timeout: Duration) -> OTelSdkResult;
   }
   ```
   1. `new` does the following, in order:
      1. `let headers = request_headers(config.otlp_headers.as_ref())?;`
      2. `let extra = parse_resource(&config.resource).map_err(|_| ObservabilityError::Resource)?;`
      3. `let resource = resource(extra);`
      4. `let client = http_client()?;`
      5. Build `OtlpLogExporter { client, url: endpoint.logs_url(), headers, resource: (&resource).into(), runtime, failures: Arc::new(FailureLog::default()) }`.
      6. `let processor = BatchLogProcessor::builder(exporter).with_batch_config(batch_config()).build();`
         - **Never** use `LoggerProviderBuilder::with_batch_exporter`, which uses the env-reading default config.
      7. `let provider = SdkLoggerProvider::builder().with_resource(resource).with_log_processor(processor).build();`
         - `build()` evaluates the env-reading `Resource::builder()` as its fallback even when a resource is given, and drops that result. Only `with_resource` decides the attributes, so it is always called.
      8. Return `LogPipeline { provider, host: endpoint.host().to_owned() }`.
   2. `bridge` returns `OpenTelemetryTracingBridge::new(&self.provider)`.
   3. `shutdown` returns `self.provider.shutdown_with_timeout(timeout)`.
   4. Write a manual `Debug` for `LogPipeline` that prints `host`, with `finish_non_exhaustive()`.
10. Add, under `#[cfg(test)]`, a `pub(crate) mod receiver` with an in-process OTLP receiver that TL1.5 also uses:
    ```rust
    /// One request the receiver accepted.
    #[derive(Debug, Clone)]
    pub(crate) struct Received {
        pub(crate) headers: axum::http::HeaderMap,
        pub(crate) request: ExportLogsServiceRequest,
    }

    /// An OTLP/HTTP logs receiver on 127.0.0.1:0 that answers `status` after `delay`.
    pub(crate) struct Receiver {
        /// The base URL to configure, `http://127.0.0.1:<port>/otlp`.
        pub(crate) base: String,
        received: Arc<Mutex<Vec<Received>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Receiver {
        pub(crate) async fn start(status: axum::http::StatusCode, delay: Duration) -> Receiver;
        /// The requests so far, in arrival order.
        pub(crate) fn received(&self) -> Vec<Received>;
        /// Every log record of every request, in order.
        pub(crate) fn records(&self) -> Vec<opentelemetry_proto::tonic::logs::v1::LogRecord>;
    }

    impl Drop for Receiver { fn drop(&mut self) { self.task.abort(); } }
    ```
    `start` binds `tokio::net::TcpListener::bind("127.0.0.1:0")` and routes only `POST /otlp/v1/logs` with `.with_state(Arc::new(ReceiverState { status, delay, received: Arc::clone(&received) }))`. `ReceiverState` is a private struct with those three fields. The handler is `async fn accept(State(state): State<Arc<ReceiverState>>, headers: HeaderMap, body: Bytes) -> StatusCode`. Its imports are `axum::extract::State`, `axum::http::{HeaderMap, StatusCode}`, `axum::routing::post`, `bytes::Bytes`, `std::io::Read as _`. The handler:
    1. sleeps `state.delay` (`tokio::time::sleep`);
    2. gunzips with `flate2::read::GzDecoder` when `content-encoding` is `gzip`, else uses the bytes as they are;
    3. decodes with `ExportLogsServiceRequest::decode`, pushes a `Received`, and answers `state.status`. An undecodable body answers 400 and pushes nothing.
    It is served with `axum::serve(listener, router)` in a spawned task.
11. Add these test helpers in `mod tests`. They are for the direct `export` tests:
    - `fn exporter_for(receiver_base: &str) -> OtlpLogExporter`. It builds the exporter as `LogPipeline::new` does (`http_client()`, `request_headers(None)`, `resource(vec![])`, `Handle::current()`), with `url` = `parse_otlp_endpoint(receiver_base).unwrap().logs_url()`.
    - The test module imports the trait as `use opentelemetry::logs::{Logger as _, LoggerProvider as _, LogRecord as _};`, because the proto type `opentelemetry_proto::tonic::logs::v1::LogRecord` is also used there by name.
    - `fn test_record(body: &'static str) -> SdkLogRecord`. It creates a provider with `SdkLoggerProvider::builder().with_resource(Resource::builder_empty().build()).build()`, then `provider.logger("test").create_log_record()` (the traits imported `as _` above), `record.set_body(body.into())` and **`record.set_observed_timestamp(std::time::SystemTime::now())`**. The transform unwraps `observed_timestamp`, which only `SdkLogger::emit` fills, so a record built by hand without it makes `export` panic.

**Tests** (in `otlp.rs` `mod tests`; async tests that block use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`):
- `records_arrive_with_fields_severity_resource_and_headers`:
  1. Start a `Receiver` with 200 and no delay.
  2. Build a `SelfMonitoringConfig` with:
     - `otlp_endpoint: Some(parse_otlp_endpoint(&receiver.base).unwrap())`;
     - `otlp_headers: Some(Secret::new("authorization=Bearer t0k3n,x-scope-orgid=7".to_owned()))`;
     - `resource: "deployment.environment=test".to_owned()`;
     - the rest default.
  3. Call `LogPipeline::new(…, Handle::current())`.
  4. Call `test_support::ensure_global_subscriber()`.
  5. Set `tracing::dispatcher::set_default(&Dispatch::new(tracing_subscriber::registry().with(pipeline.bridge())))`.
  6. Emit `tracing::warn!(project_id = 42, outcome = "failed", "flush failed")` and `tracing::error!(error = "boom", "write failed")`.
  7. Drop the guard.
  8. Run `tokio::task::spawn_blocking(move || pipeline.shutdown(Duration::from_secs(2)))`, await it, and assert the inner result `.is_ok()`.
  9. Assert:
     - at least one request arrived;
     - its headers: `authorization` `Bearer t0k3n`, `x-scope-orgid` `7`, `content-type` `application/x-protobuf`, `content-encoding` `gzip`, `user-agent` `cetra/<CARGO_PKG_VERSION>`;
     - the first resource's attributes: `service.name` `cetra`, `service.version` the crate version, `service.instance.id` non-empty, `host.arch` and `os.type` present, `deployment.environment` `test`;
     - the record with body `flush failed`: `severity_number` 13, `severity_text` `WARN`, attribute `project_id` int 42, `outcome` string `failed`;
     - the record `write failed`: severity 17 and `error` `boom`.
- `records_carry_only_the_event_fields`: in the same setup, `tracing::warn!(a = 1, b = "x", "fields")` gives a record whose attribute keys are exactly `{a, b}`: no thread name or id, and no `code.*`.
- `otel_environment_variables_change_nothing` (parent) and `env_child_exports_three_records` (child), the design's env test, without mutating the parent's environment:
  - The child is `#[ignore = "run by otel_environment_variables_change_nothing in a child process"]`. It returns at once when `std::env::var("CETRA_TEST_OTLP_CHILD_ENDPOINT")` is unset. Otherwise it:
    1. builds a pipeline to that endpoint with `otlp_headers` `x-cetra-test=1`;
    2. emits three `warn!` events named `child-1`, `child-2`, `child-3` under `set_default`;
    3. shuts down via `spawn_blocking` with 2 s.
  - The parent:
    1. starts a `Receiver`.
    2. In `spawn_blocking`, runs `std::process::Command::new(std::env::current_exe()?)` with:
       - the args `observability::otlp::tests::env_child_exports_three_records`, `--exact`, `--ignored`, `--nocapture`, `--test-threads=1`;
       - the env `CETRA_TEST_OTLP_CHILD_ENDPOINT=<receiver.base>`;
       - and `OTEL_EXPORTER_OTLP_HEADERS=x-leaked=env`, `OTEL_EXPORTER_OTLP_LOGS_HEADERS=x-leaked-logs=env`, `OTEL_RESOURCE_ATTRIBUTES=leaked.attr=1`, `OTEL_SERVICE_NAME=leaked-service`, `OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:9`, `OTEL_EXPORTER_OTLP_COMPRESSION=none`, `OTEL_BLRP_MAX_QUEUE_SIZE=1`, `OTEL_BLRP_MAX_EXPORT_BATCH_SIZE=1`, `OTEL_BLRP_SCHEDULE_DELAY=600000`.
       - Then `.output()`.
    3. Asserts:
       - the child exited successfully, and its stdout contains `1 passed`, which proves the test really ran and did not return early;
       - all three records arrived (a queue of 1 would have dropped two);
       - every request has `x-cetra-test` and `content-encoding: gzip`, and none has `x-leaked` or `x-leaked-logs`;
       - the resource has `service.name` `cetra` and no `leaked.attr`.
- `export_reports_an_error_status_as_a_failure`:
  1. Start a `Receiver` with 500 and use `exporter_for`.
  2. `let record = test_record("x");` and `let scope = opentelemetry::InstrumentationScope::builder("test").build();`.
  3. `let result = exporter.export(LogBatch::new(&[(&record, &scope)])).await;`
  4. Assert `matches!(result, Err(OTelSdkError::InternalFailure(ref reason)) if reason == "status")`. `OTelSdkError` has no `PartialEq` and holds a `String`, so never use `assert_eq!` on an `OTelSdkResult`.
- `export_reports_a_closed_port_as_a_failure`: the same with `exporter_for("http://127.0.0.1:1/otlp")`. The reason is `"connect"`.
- `outcome_classifies_each_result`:
  - `Ok(Ok(StatusCode::NO_CONTENT))` is `Ok(())`;
  - `Ok(Ok(StatusCode::TOO_MANY_REQUESTS))` is `status` with `Some(429)`;
  - the `JoinError` of a spawned task that panics is `runtime`;
  - a real `reqwest` error from `reqwest::Client::builder().no_proxy().build().unwrap().post("http://127.0.0.1:1").send().await` is `connect`.
  - Compare with `assert_eq!` on `Result<(), Failure>`: `Failure` derives `PartialEq`.
- `failures_are_logged_once_a_minute`: `should_log(t0)` true, `t0 + 59 s` false, `t0 + 60 s` true, `t0 + 61 s` false.
- `debug_hides_the_header_values`: `format!("{:?}", exporter)` of an exporter whose headers include `authorization: Bearer t0k3n` contains `authorization` and not `t0k3n`.
- `the_operator_resource_extends_and_overrides`:
  - `resource(vec![("service.name".into(), "cetra-eu1".into()), ("cetra.cell".into(), "eu1".into())])` has `service.name` `cetra-eu1`, `cetra.cell` `eu1` and `service.version` the crate version (`Resource::get(&Key::new(…))`);
  - `resource(vec![])` has exactly the five keys.
- `host_arch_and_os_type_use_semantic_convention_names`:
  - on `x86_64` `host_arch()` is `amd64`, on `aarch64` `arm64` (`#[cfg(target_arch = …)]` branches);
  - `os_type()` is `linux` on Linux;
  - neither is empty.
- `batch_config_sets_every_field`: `format!("{:?}", batch_config())` contains `max_queue_size: 2048`, `scheduled_delay: 1s` and `max_export_batch_size: 512`.
- `request_headers_put_cetras_own_last`: `request_headers(None)` has exactly `content-type`, `content-encoding` and `user-agent`. With `Some("x-a=1")` it also has `x-a`, marked `is_sensitive()`.

**Done when.** The two files are created or changed as above and committed.

**Commit:** `Add the self-monitoring OTLP log exporter`

---

### TL1.5 — The subscriber

**Goal.** One `tracing_subscriber::registry()` with a list of boxed layers: the stdout layer (output unchanged) and, when exporting, the bridge layer with its own filter. A guard flushes the export for at most 2 s on a blocking thread.

**Edit:**
- `crates/server/src/observability/subscriber.rs` (new)
- `crates/server/src/observability/mod.rs`

**Steps.**
1. In `observability/mod.rs`:
   - Add `pub mod subscriber;` after `pub mod panic;` (alphabetical order).
   - Add these variants to `ObservabilityError`, after `Resource`:
   ```rust
       /// `log.level` is not a valid filter.
       #[error("log.level: {0}")]
       LogLevel(tracing_subscriber::filter::ParseError),
       /// `self_monitoring.level` is not a valid filter.
       #[error("self_monitoring.level: {0}")]
       ExportLevel(tracing_subscriber::filter::ParseError),
       /// [`subscriber::init`] was called outside a tokio runtime.
       #[error("no tokio runtime to run the export on")]
       NoRuntime,
       /// A global subscriber is already installed.
       #[error("a global tracing subscriber is already installed")]
       AlreadyInstalled,
   ```
2. Create `subscriber.rs`. Its module doc reads:

   > The `tracing` subscriber of `cetra serve` (track TL § Architecture): one registry, one layer per sink, each with its own filter. The stdout layer is `log.level` with `rmcp` capped (unchanged output). The OTLP log layer is `self_monitoring.level` with the same cap, minus the exporter's own stack. Later milestones add their layer to [`build`]'s list and their flush to [`Observability::shutdown`]. The flush runs after `serve::run` returns, outside spec 12's 60 s shutdown bound.

   Its contracts, verbatim:
   ```rust
   /// How long [`Observability::shutdown`] waits for the export to flush.
   pub(crate) const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

   /// The `rmcp` crate logs whole MCP requests and results below `warn`; they can hold prompts
   /// and SQL text, so its level is capped whatever `log.level` says (README § Privacy).
   const RMCP_DIRECTIVE: &str = "rmcp=warn";

   /// Target prefixes the export never carries: the crates of the exporter's own stack that log
   /// per request (an export they log would be exported, and so on) and the export's failure
   /// lines. A prefix covers longer names: `hyper` covers `hyper_util`, `tower` covers
   /// `tower_http`, `http_body` covers `http_body_util`.
   const EXPORT_DROPPED_TARGETS: &[&str] = &[
       "opentelemetry",
       "hyper",
       "reqwest",
       "h2",
       "tower",
       "rustls",
       "tokio_rustls",
       "webpki",
       "want",
       "http_body",
       EXPORT_FAILURE_TARGET,
   ];

   /// One layer of the composed subscriber.
   pub(crate) type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

   /// The registry with every layer.
   pub(crate) type ComposedSubscriber = Layered<Vec<BoxedLayer>, Registry>;

   /// What the installed subscriber exports through. Dropping it does not flush: call
   /// [`Observability::shutdown`].
   #[derive(Debug)]
   pub struct Observability {
       logs: Option<LogPipeline>,
   }

   impl Observability {
       /// Whether log records are exported.
       pub fn exporting_logs(&self) -> bool;
       /// Flushes and stops the export on a blocking thread, waiting at most [`FLUSH_TIMEOUT`]
       /// (plus 500 ms for the thread hand-off). Never fails: a flush that errs or times out
       /// writes one `warn` line under [`EXPORT_FAILURE_TARGET`] (stdout only).
       pub async fn shutdown(self);
   }

   /// Builds the subscriber of `cetra serve` and installs it as the global default (with the
   /// `log` bridge). `format` is the resolved stdout format. Call it inside the tokio runtime.
   pub fn init(config: &ServerConfig, format: LogFormat) -> Result<Observability, ObservabilityError>;

   /// The subscriber and its guard, not installed. `writer` is stdout in production.
   pub(crate) fn build(
       config: &ServerConfig,
       format: LogFormat,
       writer: BoxMakeWriter,
       runtime: Handle,
   ) -> Result<(ComposedSubscriber, Observability), ObservabilityError>;

   /// `level` as an `EnvFilter`, with [`RMCP_DIRECTIVE`] added last so it replaces any
   /// `rmcp=…` directive in `level`. Shared by the stdout and export filters.
   fn capped_filter(level: &str) -> Result<EnvFilter, ParseError>;

   /// The stdout layer's filter: `log.level`, capped. Directives that only stdout must carry
   /// (M4's start-up notice) are added here, never in [`export_filter`].
   pub(crate) fn log_filter(level: &str) -> Result<EnvFilter, ParseError>;

   /// The export layer's filter: `self_monitoring.level`, capped, and nothing else.
   fn export_filter(level: &str) -> Result<EnvFilter, ParseError>;

   /// Whether an event of `target` at `level` may be exported: not under a dropped prefix, and
   /// `rmcp` at `warn` or above whatever directive matched.
   fn export_target_allowed(target: &str, level: Level) -> bool;
   ```
   (Imports:
   - `tracing::Level`;
   - `tracing_subscriber::{Layer, Registry}`, `tracing_subscriber::layer::{Layered, SubscriberExt}`;
   - `tracing_subscriber::filter::{EnvFilter, FilterExt, ParseError, filter_fn}`;
   - `tracing_subscriber::fmt::writer::BoxMakeWriter`, `tracing_subscriber::util::SubscriberInitExt`;
   - `crate::config::{LogFormat, ServerConfig}`, `super::ObservabilityError`, `super::otlp::{EXPORT_FAILURE_TARGET, LogPipeline}`.)
3. Bodies:
   1. The three filters, verbatim (bodies). `capped_filter` is the CLI's former `log_filter` body, with `ParseError` in place of `anyhow` context: the directive parse returns `ParseError` too.
      ```rust
      fn capped_filter(level: &str) -> Result<EnvFilter, ParseError> {
          let rmcp = RMCP_DIRECTIVE.parse()?;
          Ok(EnvFilter::try_new(level)?.add_directive(rmcp))
      }

      pub(crate) fn log_filter(level: &str) -> Result<EnvFilter, ParseError> {
          capped_filter(level)
      }

      fn export_filter(level: &str) -> Result<EnvFilter, ParseError> {
          capped_filter(level)
      }
      ```
      M4's TL4.7 changes `log_filter` only, to `let notice = NOTICE_DIRECTIVE.parse()?; Ok(capped_filter(level)?.add_directive(notice))`.
   2. `export_target_allowed`, verbatim:
      ```rust
      fn export_target_allowed(target: &str, level: Level) -> bool {
          if EXPORT_DROPPED_TARGETS
              .iter()
              .any(|dropped| target.starts_with(dropped))
          {
              return false;
          }
          !(target.starts_with("rmcp") && level > Level::WARN)
      }
      ```
      (`Level::DEBUG > Level::WARN` in `tracing`: more verbose is greater.)
   3. `build`, verbatim. `self_monitoring.level` is parsed first, whatever the endpoint and `logs` say. A bad value is then always a start-up error, like a bad `otlp_headers`, and it never starts a batch thread. Later milestones push their layer onto `layers` before the `Ok`.
      ```rust
      pub(crate) fn build(
          config: &ServerConfig,
          format: LogFormat,
          writer: BoxMakeWriter,
          runtime: Handle,
      ) -> Result<(ComposedSubscriber, Observability), ObservabilityError> {
          let stdout_filter = log_filter(&config.log.level).map_err(ObservabilityError::LogLevel)?;
          // No other option: the defaults are those of the former `fmt().pretty()` and
          // `fmt().json()` builders, so stdout lines are unchanged.
          let stdout: BoxedLayer = match format {
              LogFormat::Pretty => tracing_subscriber::fmt::layer()
                  .pretty()
                  .with_writer(writer)
                  .with_filter(stdout_filter)
                  .boxed(),
              LogFormat::Json => tracing_subscriber::fmt::layer()
                  .json()
                  .with_writer(writer)
                  .with_filter(stdout_filter)
                  .boxed(),
          };
          let export_level_filter = export_filter(&config.self_monitoring.level)
              .map_err(ObservabilityError::ExportLevel)?;
          let mut layers: Vec<BoxedLayer> = vec![stdout];
          let logs = match config.self_monitoring.logs_endpoint() {
              Some(endpoint) => {
                  let pipeline = LogPipeline::new(&config.self_monitoring, endpoint, runtime)?;
                  let targets = filter_fn(|metadata| {
                      export_target_allowed(metadata.target(), *metadata.level())
                  });
                  let export: BoxedLayer = pipeline
                      .bridge()
                      .with_filter(FilterExt::<Registry>::and(export_level_filter, targets))
                      .boxed();
                  layers.push(export);
                  Some(pipeline)
              }
              None => None,
          };
          Ok((tracing_subscriber::registry().with(layers), Observability { logs }))
      }
      ```
   4. `init`, in order:
      1. `let runtime = Handle::try_current().map_err(|_| ObservabilityError::NoRuntime)?;`
      2. `let (subscriber, observability) = build(config, format, BoxMakeWriter::new(std::io::stdout), runtime)?;`
      3. `subscriber.try_init().map_err(|_| ObservabilityError::AlreadyInstalled)?;`
      4. If `observability.logs` is `Some(pipeline)`, emit `tracing::info!(host = pipeline.host(), level = %config.self_monitoring.level, "self-monitoring: exporting logs over OTLP");`. It shows the host only, never the path or headers.
      5. Return the guard.
   5. `Observability::shutdown`, in order:
      1. Take `self.logs`. If it is `None`, return.
      2. `let flush = tokio::task::spawn_blocking(move || pipeline.shutdown(FLUSH_TIMEOUT));`
      3. `match tokio::time::timeout(FLUSH_TIMEOUT + Duration::from_millis(500), flush).await`:
         - `Ok(Ok(Ok(())))` does nothing.
         - On any other result, emit `tracing::warn!(target: EXPORT_FAILURE_TARGET, "self-monitoring: the log export did not flush within 2 s");`.
         - No error text. The SDK's error can hold `Debug` of internals.
   6. `exporting_logs` is `self.logs.is_some()`.

**Tests** (in `subscriber.rs` `mod tests`). Every test builds its `ServerConfig` with `crate::config::load_from(toml, &[])`, which TL1.2 makes `pub(crate)`: `load_from(None, &[])` for the defaults, or a TOML such as `"[log]\nlevel = \"info\"\n[self_monitoring]\notlp_endpoint = \"<base>\"\nlevel = \"warn\"\n"`. `ServerConfig` has no `Default`.
- `log_filter_caps_rmcp`: moved from `crates/cli/src/main.rs` unchanged, except that it calls the local `log_filter`.
- `a_debug_filter_drops_rmcp_debug_events`: moved from the CLI with its `Buffer` writer, adapted to the local `log_filter`.
- `export_filter_caps_rmcp`: for `warn`, `info,rmcp=trace` and `trace`, the filter's `to_string()` contains `rmcp=warn` and not `rmcp=trace`.
- `export_filter_adds_only_the_rmcp_cap`: the comma-separated directives of `export_filter("warn").unwrap().to_string()`, sorted, are exactly `["rmcp=warn", "warn"]`. A directive added to `log_filter` later (M4) must never show here.
- `export_target_allowed_drops_the_exporter_stack`, a table:
  - false: `opentelemetry_sdk`, `opentelemetry_appender_tracing`, `hyper_util::client::legacy`, `hyper::proto`, `reqwest::connect`, `h2::codec`, `tower::buffer`, `tower_http::trace`, `rustls::conn`, `tokio_rustls::client`, `webpki::verify`, `want`, `http_body_util`, `cetra_self_monitoring` at `WARN`; `rmcp::service` at `DEBUG` and `INFO`;
  - true: `rmcp::service` at `WARN` and `ERROR`, `cetra_server::serve` at `INFO`, `cetra_storage::wal` at `ERROR`.
- `stdout_json_matches_the_former_builder`:
  1. Set `tracing::dispatcher::set_default` once with `tracing_subscriber::fmt().json().with_writer(buffer_a).finish()` and once with `build`'s subscriber, using `load_from(None, &[])` (`log.level` `info`) and `buffer_b` as the `BoxMakeWriter`.
  2. Emit `tracing::info!(project_id = 7, "same line")` from the same source line in both: wrap it in a helper fn `emit()` called twice.
  3. Parse each output line as `serde_json::Value`, remove `timestamp`, and assert equality.
- `stdout_pretty_matches_the_former_builder`: the same with `.pretty()`. Compare the outputs with the timestamp, the first whitespace-separated token of the first line, removed.
- `no_endpoint_means_no_export_layer`: `build` with `load_from(None, &[])` and `Handle::current()` gives `exporting_logs() == false`, and the subscriber has one layer (`subscriber` is `Layered`; checking `exporting_logs` is enough).
- `logs_false_means_no_export_layer`: an endpoint set with `logs = false` gives `exporting_logs() == false`.
- `an_invalid_export_level_is_an_error`: `self_monitoring.level = "cetra_server=nonsense"` fails with `ObservabilityError::ExportLevel(_)`, whose `Display` starts with `self_monitoring.level: `. Check three configs: an endpoint set, no endpoint, and an endpoint with `logs = false`.
- `an_invalid_log_level_is_an_error`: `log.level = "cetra_server=nonsense"` gives `ObservabilityError::LogLevel(_)`.
- `the_export_layer_applies_its_own_filter` (multi-thread):
  1. Start a `Receiver` (200, no delay) and build with `log.level = "info"`, `self_monitoring.level = "warn"`, the endpoint `receiver.base`, and a buffer writer.
  2. Call `test_support::ensure_global_subscriber()`, then set the subscriber with `set_default`.
  3. Emit:
     - `info!("info-canary")` and `warn!("warn-canary")`;
     - `debug!(target: "rmcp::service", "rmcp-canary")` and `warn!(target: "rmcp::service", "rmcp-warn-canary")`;
     - `warn!(target: "hyper_util::client", "hyper-canary")` and `warn!(target: "cetra_self_monitoring", "failure-canary")`.
  4. Drop the guard, then `observability.shutdown().await`.
  5. Assert that the received record bodies are exactly `{warn-canary, rmcp-warn-canary}`.
  6. Assert that the stdout buffer contains `info-canary`, `warn-canary`, `hyper-canary` and `failure-canary`, and not `rmcp-canary`.
- `shutdown_returns_within_the_flush_timeout_when_the_endpoint_hangs` (multi-thread): with a `Receiver` that answers after 30 s, emit one `warn!`, then time `observability.shutdown().await`. It returns in under 3 s.
- `shutdown_without_export_returns_at_once`: a guard with no pipeline returns from `shutdown` immediately.

**Done when.** The two files are created or changed as above and committed.

**Commit:** `Build the cetra serve subscriber with an OTLP log layer`

---

### TL1.6 — Wire the CLI

**Goal.** `cetra serve` installs the panic hook before the config loads, installs the subscriber through `observability::subscriber::init`, exports its own error, and flushes the export on both exits.

**Edit:**
- `crates/cli/src/main.rs`
- `crates/cli/Cargo.toml`
- `Cargo.lock` (rewritten by `cargo metadata`)

**Steps.**
1. In `main.rs`, delete:
   - `RMCP_DIRECTIVE`, `log_filter` and `init_tracing`;
   - the import `use tracing_subscriber::EnvFilter;`;
   - `LogConfig` from the `cetra_server::config` import, keeping `LogFormat` and `ServerConfig`;
   - the tests `log_filter_caps_rmcp` and `a_debug_filter_drops_rmcp_debug_events` with their `Buffer` type, now in `subscriber.rs`;
   - `log_filter` from the tests' `use super::{…}`;
   - `use std::sync::{Arc, Mutex};` in the test module (line ~956). Only the moved tests and `Buffer` used it. `Path` and `PathBuf` stay used.
   Keep `default_log_format`.
2. Replace `serve` with:
   ```rust
   async fn serve(config_path: Option<&Path>) -> anyhow::Result<()> {
       // Before the config loads, so a panic while loading it is reported too (track TL).
       observability::panic::install();
       let config = ServerConfig::load(config_path)
           .map_err(ServeError::from)
           .context("loading config")?;
       let format = config.log.format.unwrap_or_else(default_log_format);
       let observability =
           observability::subscriber::init(&config, format).context("loading config")?;
       let result = run_server(config).await;
       if let Err(error) = &result {
           // The chain stderr prints, also exported (track TL § Lifecycle).
           let chain = format!("{error:#}");
           tracing::error!(error = %chain, "cetra serve failed");
       }
       observability.shutdown().await;
       result
   }

   async fn run_server(config: ServerConfig) -> anyhow::Result<()> {
       let signals = Signals::install().context("installing signal handlers")?;
       serve::run(config, Some(cetra_ui::router()), signals.wait())
           .await
           .map_err(annotate)?;
       Ok(())
   }
   ```
   Add `use cetra_server::observability;` to the imports. If clippy's `large_futures` concerns are visible in the existing code, for example a `Box::pin` around `serve::run`, keep them as they are. `serve::run` already boxes `bind`.
3. In `crates/cli/Cargo.toml`, delete `tracing-subscriber.workspace = true` from `[dependencies]`, because the CLI no longer uses it. Add `flate2.workspace = true` to `[dev-dependencies]` in alphabetical position.
4. Run `cargo metadata --format-version 1 --offline > /dev/null`. `git diff Cargo.lock` must show only `cetra-cli`'s dependency list changing (`tracing-subscriber` out, `flate2` in).

**Tests** (in `main.rs` `mod tests`):
- `serve_exports_its_start_up_error_before_exiting` (`#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`):
  1. Start a minimal OTLP receiver in the test. The server crate's `Receiver` is `#[cfg(test)]` there and not reachable from the CLI.
     - It is an `axum` router on `tokio::net::TcpListener::bind("127.0.0.1:0")` with `POST /otlp/v1/logs`, served by `axum::serve` in a spawned task.
     - State: `.with_state(Arc::clone(&received))`, where `received: Arc<std::sync::Mutex<Vec<ExportLogsServiceRequest>>>`.
     - Handler: `async fn(State(received): State<Arc<Mutex<Vec<ExportLogsServiceRequest>>>>, body: bytes::Bytes) -> StatusCode`. It gunzips with `flate2::read::GzDecoder` (`std::io::Read::read_to_end`), decodes with `prost::Message::decode`, pushes, and answers 200.
     - Imports: `axum::{Router, extract::State, http::StatusCode, routing::post}`, `bytes::Bytes`, `flate2::read::GzDecoder`, `opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest`, `prost::Message`, `std::sync::{Arc, Mutex}`. All are CLI dev-dependencies (`flate2` from step 3).
  2. Hold a `std::net::TcpListener` on `127.0.0.1:0` so its port is taken.
  3. Write a `cetra.toml` in a `tempfile::tempdir()`, like `test_server.rs`:
     - `data_dir` in the temp dir;
     - `http_addr` = the taken address;
     - `otlp_http_addr = ""` and `grpc_addr = ""`;
     - `[self_monitoring]` with `otlp_endpoint = "http://<receiver addr>/otlp"`.
  4. `run_cli(["cetra", "serve", "--config", <path>]).await` is `Err`, and its chain contains `binding`.
  5. The receiver got at least one record with body `cetra serve failed`, severity 17, and an `error` attribute containing `binding`.
  6. Comment in the test that it installs the process's global subscriber and panic hook. Under nextest each test is its own process. Under `cargo test`, it is the only CLI test that reaches `subscriber::init`: `missing_config_exits_with_the_path` also runs `serve`, but it stops at the config load and only installs the (idempotent) panic hook.

**Done when.** The three files are changed as above and committed. The CLI test runs its server crate without `cfg(test)`, so its export client honours `HTTP(S)_PROXY`. Like the CLI's other 127.0.0.1 tests, it needs `NO_PROXY` to cover 127.0.0.1 when a proxy is set. The coordinator also checks by hand, at the milestone check, that `just run serve` with `CETRA_LOG=info` prints the same stdout lines as before the change, in both formats (`CETRA_LOG__FORMAT=json` and `=pretty`).

**Commit:** `Export cetra serve logs over OTLP and flush them at exit`

---

### TL1.7 — Docs, spec 12 and changelog

**Goal.** Operators can set up the export from the Monitoring page. Spec 12 and the CHANGELOG describe what M1 built. The D38 and D-N-33 notes are the coordinator's.

**Edit:**
- `website/src/content/docs/docs/self-hosting/monitoring.md`
- `website/src/content/docs/docs/self-hosting/backup.md` (§ Restore mode only)
- `website/src/content/docs/docs/self-hosting/configuration.md` (the `restore_mode` row only)
- `CHANGELOG.md`
- `docs/spec/12-operations.md`

**Steps.**
1. `monitoring.md`:
   - Change the frontmatter `description` to `"Check cetra's health, scrape its Prometheus metrics, export its logs over OTLP and set up alerts."`, which is under 160 characters.
   - Change the first paragraph to say that cetra also exports its own logs to an OTLP endpoint when configured, and that all three are described here.
   - Insert this section **before** `## Alert examples`. Keep the exact heading text: the configuration reference links to `#export-logs-over-otlp`.
   ````markdown
   ## Export logs over OTLP

   cetra can send its own log events (warnings and errors by default) to an OTLP/HTTP endpoint you
   choose: Sentry, Grafana Cloud, an OpenTelemetry Collector, or any backend that accepts OTLP logs
   over HTTP. It is off until you set `self_monitoring.otlp_endpoint`.

   ```toml
   [self_monitoring]
   otlp_endpoint = "https://otlp.example.com/otlp"   # cetra posts to <endpoint>/v1/logs
   otlp_headers = "authorization=Bearer <token>"     # a secret; name=value pairs, comma-separated
   level = "warn"                                    # which events are exported
   resource = "deployment.environment=prod"          # extra resource attributes
   ```

   The environment form of each key is `CETRA_SELF_MONITORING__` followed by the key in upper case,
   for example `CETRA_SELF_MONITORING__OTLP_ENDPOINT`. Every key is in the
   [configuration reference](/docs/self-hosting/configuration-reference/#self_monitoring).

   - **What is sent.** One OTLP log record per event that passes `level`: the message, the event's
     fields, its level, its target (as the scope name) and its event name (`event <file>:<line>`
     of cetra's source). The resource holds `service.name = "cetra"`,
     `service.version`, `service.instance.id` (new at each start), `host.arch`, `os.type`, and the
     attributes of `resource`, which replace cetra's own on the same key. The records carry what the
     server's stdout carries, and stdout never carries prompts, completions, attribute values,
     secrets, API keys or email addresses. `rmcp` events are capped at `warn`, as for stdout.
   - **How.** Protobuf, gzip-compressed, `POST <otlp_endpoint>/v1/logs`, batched every second, with
     the headers of `otlp_headers`. Redirects are not followed; a request times out after 10 s. A
     failed export prints one warning on stdout at most once a minute and is not retried.
   - **Panics** also appear as an ERROR event `panic` with the code location. The panic message
     stays on stderr only.
   - **At shutdown**, cetra flushes what is queued for at most 2 s, also when it fails to start (the
     start-up error is exported as `cetra serve failed`).
   - **Restore mode** does not stop the export: it goes to your own endpoint.
   - **No `OTEL_*` variable is read.** Variables such as `OTEL_EXPORTER_OTLP_HEADERS` set for other
     programs on the host change nothing.
   - **Another cetra instance is not a destination for these logs**: cetra drops the OTLP logs it
     receives. Send them to a logs backend, or through a Collector.

   ### Sentry

   In Sentry, open your project's settings, then **Client Keys (DSN)**. The DSN looks like
   `https://<public key>@o<org id>.ingest.<region>sentry.io/<project id>`; organizations in the EU
   have `ingest.de.sentry.io`. Use its parts:

   ```bash
   CETRA_SELF_MONITORING__OTLP_ENDPOINT=https://o<org id>.ingest.de.sentry.io/api/<project id>/integration/otlp
   CETRA_SELF_MONITORING__OTLP_HEADERS="x-sentry-auth=sentry sentry_key=<public key>"
   ```

   The records should arrive as Sentry logs (check in your project).

   ### Grafana Cloud

   In your Grafana Cloud stack, open **Connections**, then **OpenTelemetry (OTLP)**. It shows the OTLP
   endpoint of your stack's region and generates a token. The header is `Authorization: Basic` with
   the base64 of `<instance id>:<token>`; the `%20` form that Grafana's snippet shows works too:

   ```bash
   CETRA_SELF_MONITORING__OTLP_ENDPOINT=https://otlp-gateway-<region>.grafana.net/otlp
   CETRA_SELF_MONITORING__OTLP_HEADERS="Authorization=Basic%20<base64 of instance id:token>"
   ```

   ### OpenTelemetry Collector

   Point cetra at the Collector's OTLP/HTTP receiver (port 4318) and let the Collector add
   authentication, filter or forward to several backends:

   ```bash
   CETRA_SELF_MONITORING__OTLP_ENDPOINT=http://otel-collector:4318
   ```

   ```yaml
   receivers:
     otlp:
       protocols:
         http:
           endpoint: 0.0.0.0:4318
   exporters:
     debug: {}
   service:
     pipelines:
       logs:
         receivers: [otlp]
         exporters: [debug]
   ```

   Replace the `debug` exporter with your backend's. The endpoint may be a private address:
   self-monitoring is operator configuration and skips the private-network egress guard.
   ````
   Every `CETRA_SELF_MONITORING__*` name used is a documented key, as `check-env-vars.mjs` requires. No banned term is used (`check-terms.mjs`). `toml`, `bash` and `yaml` fences need no sample marker.
2. Restore mode keeps the export to the operator's own endpoint (design § The switchboard, rule 3). Say so where restore mode is described:
   1. `backup.md` § Restore mode:
      - Replace `starts a server that makes no outbound call.` with `starts a server that makes no outbound call, except the self-monitoring export to your own endpoint ([Monitoring](/docs/self-hosting/monitoring/#export-logs-over-otlp)).`
      - Append this bullet as the last item of the list under `With restore mode on, the server:`: `- keeps exporting its own logs to \`self_monitoring.otlp_endpoint\`, when it is set: that endpoint is yours.` Keep it last; M4 may add its own bullet after it.
   2. `configuration.md`, the `restore_mode` row: replace `no outbound calls, for restore drills` with `no outbound calls except the self-monitoring export, for restore drills`, and keep the link.
   3. `docs/spec/12-operations.md`, the configuration table's `restore_mode` row (line ~18): append ` The self-monitoring export to the operator's own endpoint stays on (D-TL-1).` at the end of the third cell, before its closing `|`. M4's TL4.10 appends its own clause to the same row: append, never rewrite.
3. `CHANGELOG.md` § `[Unreleased]` § `### Added`, appended as the last item:
   `- \`cetra serve\` can export its own logs to an OTLP/HTTP endpoint you choose, such as Sentry, Grafana Cloud or an OpenTelemetry Collector: set \`self_monitoring.otlp_endpoint\` and usually \`self_monitoring.otlp_headers\`. Off by default; warnings and errors are exported unless \`self_monitoring.level\` says otherwise. A panic is also logged as an ERROR event with its code location.`
4. `docs/spec/12-operations.md` (in addition to step 2.3):
   1. In the configuration table, after the `log.format` / `log.level` row, add the row `` | `self_monitoring.otlp_endpoint` / `self_monitoring.otlp_headers` | | unset (off) / unset. Base `http(s)` URL without user info, query or fragment, `/v1/logs` appended; headers `name=value,…` (secret, percent-decoded, ≤ 32 and 8192 bytes) (D-TL-1) | `` and the row `` | `self_monitoring.logs` / `self_monitoring.level` / `self_monitoring.resource` | | `true` / `"warn"` / `""` (`key=value,…` resource attributes) | ``. Match the table's column count by reading its header.
   2. Replace the sentence `cetra has no \`tls.*\` keys and no telemetry key: the loader refuses them as unknown keys.` with exactly `cetra has no \`tls.*\` keys: the loader refuses them as unknown keys. Its own logs are exported with \`[self_monitoring]\` (§ Self-monitoring, D-TL-1).` This makes no claim about `telemetry.*`, which M4 adds.
   3. § Process lifecycle, the Shutdown bullet: after `close SQLite.` add `Then the self-monitoring export is flushed (≤ 2 s, on a blocking thread), also when start-up fails after the config loaded (track TL M1).` After `the pending-map, … SQLite close are short meta writes outside that bound` add `, and so is the self-monitoring flush`.
   4. § Self-monitoring: after the N M3 block, add:
      ```markdown
      > **Built in TL M1 (D-TL-1 to D-TL-3):**
      > - `[self_monitoring]` exports `tracing` events that pass `self_monitoring.level` (default `warn`, `rmcp` capped at `warn`) as OTLP log records, protobuf and gzip over HTTP, to `<otlp_endpoint>/v1/logs`, with the operator's `otlp_headers`. Redirects off, 10 s timeout, batches of up to 512 records every second, queue of 2048. No `OTEL_*` variable is read (the resource starts empty; every batch setting is explicit).
      > - Resource: `service.name = "cetra"`, `service.version`, `service.instance.id` (UUIDv7 per process start), `host.arch`, `os.type`, then `self_monitoring.resource`.
      > - The export drops the exporter's own stack (`opentelemetry*`, `hyper*`, `reqwest`, `h2`, `tower*`, `rustls*`) and its failure lines (target `cetra_self_monitoring`, logged on stdout at most once a minute).
      > - The panic hook of `cetra serve` chains the default hook and emits an ERROR event `panic` with the location only, never the message.
      > - The export runs in restore mode too: it goes to the operator's own endpoint (D-N-25 covers outbound work for tenants and to us).
      ```
      Replace `OTel self-traces stay deferred (D38).` with `Server spans over OTLP: track TL M2 (D-TL-4).`

**Tests.** The coordinator's `just check` runs `check-docs` on these pages:
- terms and description length;
- env-var names: every `CETRA_SELF_MONITORING__*` must be a documented key, which TL1.2 adds;
- the link and anchor `#export-logs-over-otlp` and `#self_monitoring`.
The implementer checks by reading that the anchor in `configuration-reference.md` (`#export-logs-over-otlp`) matches the heading, and that `#self_monitoring` is the slug of the heading `` ## `[self_monitoring]` `` (Starlight drops the backticks and brackets). If unsure about the slug, link to `/docs/self-hosting/configuration-reference/` without an anchor and report it.

**Done when.** The five files are changed as above and committed.

**Commit:** `Document the self-monitoring log export`

---

## Milestone check (coordinator)

After TL1.7 is merged with whatever M4 phases ran:
1. `just fmt`. The plan's verbatim code is written to be rustfmt-clean, and the phases run `rustfmt` on their leaf files, but this covers `lib.rs`, the `mod.rs` files, `main.rs` and the rest.
2. `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check`. Run it with no `HTTP(S)_PROXY`, or with `NO_PROXY` covering 127.0.0.1: the CLI test's export client honours proxies.
3. `just deny` (the licences and single versions of the opentelemetry family)
4. By hand:
   - `just run serve` prints the same stdout lines as before, in `pretty` and `json`.
   - With an OpenTelemetry Collector (`otel/opentelemetry-collector` with the `debug` exporter above) and `CETRA_SELF_MONITORING__OTLP_ENDPOINT=http://127.0.0.1:4318`, a `warn` line, for example from a refused login, appears in the Collector's output with the resource attributes.
   - `kill -TERM` flushes it.
5. Update `docs/roadmap.md`'s track TL row (the coordinator's job, AGENTS.md § Parallel work).
