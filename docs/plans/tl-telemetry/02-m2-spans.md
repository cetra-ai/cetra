# TL M2 — Server spans and trace export

**Summary.** This milestone adds cetra's own traces to the self-monitoring export of M1. It adds:
- two `[self_monitoring]` keys: `traces` (bool, default `true`) and `trace_sample_ratio` (0.0–1.0, default `0.1`, validated);
- an OTLP/HTTP **span** exporter beside M1's log exporter in `observability/otlp.rs`. It shares M1's request path (gzip, headers, timeout, no redirects, failure throttle), refactored into one `post` function. It runs behind a `BatchSpanProcessor` and an explicit `ParentBased(TraceIdRatioBased(ratio))` sampler, with every batch and limit setting given, and the same env-free resource as the logs;
- a `tracing-opentelemetry` 0.34.0 layer in M1's subscriber, with its own filter: cetra's spans only, plus the events the log export would carry;
- a stdout layer that no longer sees spans. The stdout lines of events outside dependency spans (all of cetra's own events) stay byte-identical, and a test proves it. An event that a dependency emits inside its own span (an `rmcp` warning inside an rmcp span) loses the span context that the formatter used to print;
- the spans themselves, built by constructor functions in one catalog file per crate (`observability/spans.rs` in the server, `spans.rs` in storage), with constant keys and id, count, enum and duration values only (D-TL-4): HTTP requests, OTLP ingest (HTTP and gRPC), SQL console and MCP queries, eval jobs, alert evaluation and delivery, deletions, retention, issue candidates, backups, and storage flush, compaction and GC;
- docs: the Monitoring page, the configuration reference, the CHANGELOG, spec 12, and one row of M4's Telemetry page.

Incoming `traceparent` headers are ignored (D-TL-7).

**Prerequisites.**
- M1 (`docs/plans/tl-telemetry/01-m1-self-monitoring-logs.md`) and M4 (`04-m4-heartbeat.md`) are committed, compile, and are clippy-clean (last fix: `24313c4`).
- M2 builds on M1's code:
  - in `config.rs`: `SelfMonitoringConfig` and `OtlpEndpoint`;
  - in `observability/otlp.rs`: `OtlpLogExporter`, `LogPipeline`, `FailureLog`, `Failure`, `outcome`, `gzip`, `http_client`, `request_headers`, `resource`, and the test module `receiver`;
  - in `observability/subscriber.rs`: `build`, `init`, `Observability`, `export_filter`, `export_target_allowed`, `FLUSH_TIMEOUT`, `EXPORT_FAILURE_TARGET`.
- These names were checked against the committed code. Each phase lists the names it relies on. **If one differs, stop and report.**
- M2 uses none of M4's code. M4's heartbeat reads `SelfMonitoringConfig::exports()`, which M2 widens to traces.
- Work in `/home/sebastien/travaux/llm-observer/.claude/worktrees/cross-track-review-6a41ad`, branch `claude/telemetry-9e2a24`.

**Choices this plan applies.** The design records them: `README.md` § "Details settled while planning", bullet "Server spans (M2)".
- Events recorded on spans pass the log-export filter.
- `with_error_events_to_status` stays off. An ERROR event still marks its span failed.
- `/healthz` and `/readyz` get no span.
- No per-read `query` span; the SQL console and MCP tool calls get one.
- One span per unit of work (`issues.candidate`, not `issues.pass`).
- Ratio 0 builds no span layer.
- No trace ids in log records.
- `http.request` ends at the response head, and a 5xx marks it failed.
- `trace_sample_ratio` is a validated newtype.

**Parallel milestones and shared files.** M3 (debug bundle), M5 (crash reports) and M6 (product analytics) are implemented in parallel with M2 in the same checkout. They may land first in a file M2 also edits:
- `observability/subscriber.rs` (`build`, `Observability`, `shutdown`);
- `observability/otlp.rs`, `observability/mod.rs`;
- `config.rs`, `http/mod.rs`, `routes/sql.rs`, `mcp/mod.rs`;
- `CHANGELOG.md`, `docs/spec/12-operations.md`, the configuration reference, `telemetry.md`.

Every M2 edit to these files is described as an addition, or as a replacement of a named M1 line. **Keep whatever another milestone added**: its layers, fields, `mod` lines, `shutdown` steps, config fields, rows, CHANGELOG items, spec blocks and handler lines.
- When a line this plan replaces has been changed by another milestone, apply M2's change on top of it, and say so in the report.
- When you cannot tell which milestone wrote a line, stop and report.

The coordinator serializes the phases that share a file.

**Notes for M3 and M5** (also sent to the coordinator):
- Every layer they add needs a per-layer filter that passes events only (`metadata.is_event()`), or a filter of their own. Otherwise:
  - an M3 ring-buffer line inside a span gains `span`/`spans` keys and differs from stdout;
  - an unfiltered layer enables every span of every crate, which defeats the `cetra_`-only span filter.
- M3's config allow-list classifies `self_monitoring.traces` and `self_monitoring.trace_sample_ratio`. Both are non-secret.

## Read first

- `AGENTS.md`, all of it, especially § Rules, § Privacy, § Tests, § Commits and reports.
- `docs/plans/tl-telemetry/README.md`:
  - § Architecture;
  - § 1 Self-monitoring over OTLP (Mechanism, Signals, Resource, Privacy, Feedback loops, Lifecycle);
  - § 2 Server spans;
  - § Details settled while planning;
  - § Decisions D-TL-2, D-TL-3, D-TL-4, D-TL-7;
  - § Milestones M2.
- `docs/plans/tl-telemetry/01-m1-self-monitoring-logs.md`:
  - § Interface for later milestones;
  - TL1.2 (the config code);
  - TL1.4 (`otlp.rs`: the exporter, `post`'s predecessor in `export`, the `receiver` module);
  - TL1.5 (`subscriber.rs`: `build`, `init`, `Observability::shutdown`, the filters, the tests);
  - TL1.7 (the Monitoring page section, spec 12).
- The code M2 changes (the paths are under `crates/`):
  - `server/src/http/mod.rs`: `log_request`, `finish`.
  - `server/src/routes/otlp.rs`: `export`, `export_body`; the tests `post_traces`, `plain_export`, `protobuf`.
  - `server/src/ingest/grpc.rs`: `GrpcTraces::export`, `export_request`, `refused`; the tests `send`, `traces`, `span_count`, `bearer`.
  - `server/src/routes/sql.rs`: `run_sql`, `run_json`, `start_export`, `log_end`, `log_failure`, `failure_kind`; the tests `loaded`, `post_sql`.
  - `server/src/mcp/mod.rs`: `mcp`, `McpServer`, `call_tool`, `run_tool`; the test helpers `call`, `app_with_token`.
  - `server/src/eval/dispatch.rs`: `run_claimed`.
  - `server/src/alerts/evaluate.rs`: `evaluate_once`, `evaluate_rule`.
  - `server/src/alerts/deliver.rs`: `deliver_once`, `deliver_one`.
  - `server/src/deletion/mod.rs`: `run_once`, `Step`.
  - `server/src/retention.rs`: `retention_once`, `RetentionRun`.
  - `server/src/issues/worker.rs`: `run_candidate`.
  - `server/src/backup.rs`: `run`, `BackupReport`, `BackupMode`, `BackupError::code`.
  - `server/src/telemetry.rs`: `Method::of`, `Protocol`, `IngestOutcome`, `ingest_outcome`. They are reused, not changed.
  - `storage/src/flush.rs`: `run`, `flush_once`, `log_retry`; the tests `anchor_config`, `project`, `day`, `at`, `spans_named`.
  - `storage/src/compaction/job.rs`: `Compactor::pass`, `kind_label`, `CompactionReport`.
  - `storage/src/gc.rs`: `spawn`, `collect_once`, `collect_obsolete`, `ObsoleteReport`.
  - `storage/src/lib.rs`: the `mod` list (lines 46–76), `Storage::flush_now`.
- Registry sources under `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`:
  - `tracing-opentelemetry-0.34.0/src/layer.rs`:
    - `OpenTelemetryLayer::new` (line ~660) and its `with_*` setters (~700–905);
    - `parent_context` (~912);
    - `on_new_span`, `on_event` and `on_close` (~1136–1530);
    - `SpanEventVisitor` (~256) and `SpanAttributeVisitor` (~498). Note: `record_u64` falls back to `record_debug`, so a `u64` becomes a string attribute. Counts are recorded as `i64` (`spans::count`).
    - The special fields `otel.kind` and `otel.status_code` (lines 30–33, ~539).
  - `opentelemetry_sdk-0.33.1/src/trace/`:
    - `export.rs`: `SpanExporter`, `SpanData`;
    - `span_processor.rs`: `BatchSpanProcessor::builder`, `BatchConfigBuilder` and its env reading (~1060–1240);
    - `provider.rs`: `TracerProviderBuilder`, `with_sampler`, `with_span_limits`, `with_resource`, `build`, `shutdown_with_timeout`;
    - `config.rs`: `Config::default` reads `OTEL_TRACES_SAMPLER*` and `OTEL_SPAN_*_LIMIT`, so both are overridden;
    - `span_limit.rs`;
    - `sampler.rs`: `Sampler`, `sample_based_on_probability`.
  - `opentelemetry-proto-0.33.1/src/transform/trace.rs` line ~176: `tonic::group_spans_by_resource_and_scope`.
  - `tracing-0.1.44/src/lib.rs` lines 322–332: constant field names `{ NAME } = value`.
  - `tracing-subscriber-0.3.23/src/layer/context.rs`: `lookup_current`. A layer's context skips the spans its per-layer filter disabled. This is why a stdout layer that disables spans prints no span context.
  - `tracing-subscriber-0.3.23/src/fmt/format/pretty.rs` lines ~296–330: the `in <span>` lines.
  - `tracing-subscriber-0.3.23/src/fmt/fmt_layer.rs` lines ~640–690: `with_current_span`, `with_span_list`.

## What it is, and what it is not

**What works when it's done:**
- With `self_monitoring.otlp_endpoint` set, `traces = true` (the default) and `trace_sample_ratio > 0` (default `0.1`), `cetra serve` exports the spans of § The spans to `<otlp_endpoint>/v1/traces`. The export is protobuf and gzip, with M1's headers, no redirects and a 10 s timeout. It is batched every second.
- Each root span is kept with probability `trace_sample_ratio`, and its children follow it (`ParentBased(TraceIdRatioBased)`).
- The resource is M1's, with the same `service.instance.id` as the log records.
- No `OTEL_*` variable changes the span export. A child-process test proves it, as M1's does for logs.
- Span attributes are exactly the keys declared in the span catalog. There are no thread, code-location, target or busy/idle attributes.
- An ERROR event inside a span sets its status to error. A 5xx response sets the `http.request` span's status to error.
- Events that pass the log-export filter are recorded on the enclosing span, with their fields.
- Only spans whose target starts with `cetra_` are exported. Spans of other crates are never exported: `rmcp`'s spans carry whole MCP requests.
- An incoming `traceparent` is ignored: a request span always starts a new trace.
- Stdout lines of events outside dependency spans are byte-identical with spans present, in both formats. That covers every event cetra emits, including those inside cetra's own spans. A dependency's event inside the dependency's own span no longer prints that span's context (json `span`/`spans`, pretty `in` lines), as the design's "stdout layer without span fields" implies.
- At exit, the log and span exports flush in parallel, together within `FLUSH_TIMEOUT` plus 500 ms.
- The docs, the configuration reference, spec 12 and the CHANGELOG describe it.

**Out of scope:**
- Trace ids in log records (design: out of scope).
- Per-read storage spans (design: the request span covers the read API).
- Metrics over OTLP (D-N-28).
- Spans in commands other than `cetra serve`. The CLI's other commands install no traces layer.
- Propagating context into outbound calls (judges, webhooks): no propagator exists (D-TL-7).
- The D38 and D-N-33 notes and the roadmap row: the coordinator's.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-TL-1 | `[self_monitoring]` config table; no `OTEL_*` variables |
| D-TL-2 | OpenTelemetry Rust SDK 0.33.1 plus `tracing-opentelemetry` 0.34.0; cetra's own OTLP/HTTP exporters on `opentelemetry-proto`'s `transform`, `prost`, `flate2`, `reqwest` 0.12; every batch, sampler and limit setting explicit; `Resource::builder_empty()` |
| D-TL-3 | The export carries what stdout carries, with the same caps; traces default to 10 % |
| D-TL-4 | Span names and attribute keys are constants; values are ids, counts, enums and durations; `info_span!` with explicit fields, never `#[instrument]` |
| D-TL-7 | Incoming `traceparent` is ignored: no propagator, nothing extracted |
| D150 | No `axum/tracing`, no `tower-http::TraceLayer`; the route is the matched template |
| D-G-40 | cetra stores OTLP traces and drops OTLP logs: another cetra can receive the traces |
| D-N-28 | Prometheus `/metrics` stays the metrics interface |
| D166 | The endpoint is operator configuration: no SSRF guard |

The design's § "Details settled while planning" applies, including its bullet "Server spans (M2)". Plan-level details:
- **Attribute names.** HTTP spans use the OTel semantic-convention keys (`http.request.method`, `http.route`, `http.response.status_code`) and the `tracing-opentelemetry` special fields `otel.kind` and `otel.status_code`. Every other key is a plain name that matches the existing log fields (`project_id`, `outcome`, `spans`, …).
- **Counts are `i64`.** `tracing-opentelemetry` records `u64` values as strings, so every count goes through `spans::count`.
- **Batch and limit settings.** Span batches use the log batches' values: queue 2048, delay 1 s, batch 512. The span limits are the SDK's defaults, written out: 128 events and 128 attributes per span, 128 links, 128 attributes per event and per link.
- **`service.instance.id`** is created once per process in a `OnceLock`, so the log and span resources built by `resource()` are equal.
- **The layer's settings.** Location, threads, target, level, busy/idle tracking, OTel context activation and every exception mapping are off.
  - With context activation off, a span's parent is the enclosing `tracing` span that this layer sees, or the explicit `parent:`.
  - With the exception mappings off, an event's attributes are exactly its fields plus the `level` and `target` that the layer always adds to span events.

## Versions

Checked on 2026-11-23 with `cargo info` and the registry sources. The implementer re-checks with `cargo info <crate>` and reports the versions used.

| Crate | Version | Features | Where | Why |
|---|---|---|---|---|
| `tracing-opentelemetry` | 0.34.0, the latest (released 2026-09-20). **New** in `Cargo.lock`. | `default-features = false`, none. The default set `["tracing-log", "metrics"]` is unused. | root `[workspace.dependencies]`; `cetra-server` | `OpenTelemetryLayer`: `tracing` spans to OTel spans. It requires `opentelemetry ^0.33.0` (feature `trace`, no defaults) and `tracing-subscriber ^0.3.22` (`registry`, `std`); the lock has 0.33.1 and 0.3.23. It also requires `tracing 0.1.35` and `tracing-core 0.1.28`; the lock has 0.1.44 and 0.1.36. Licence MIT. |
| `opentelemetry` | 0.33.1, unchanged | add `"trace"` to M1's `["logs"]` | root | `opentelemetry::trace::TracerProvider` |
| `opentelemetry_sdk` | 0.33.1, unchanged | add `"trace"` to M1's `["logs"]` | root | `SdkTracerProvider`, `BatchSpanProcessor`, `Sampler`, `SpanLimits`, `SpanExporter`. `trace` pulls `rand` 0.9 and `percent-encoding`, which `opentelemetry-proto`'s `trace` feature already brings: the lock's `opentelemetry_sdk` entry already lists both. |
| `opentelemetry-proto` | 0.33.1, unchanged | already has `trace` | unchanged | `transform::trace::tonic::group_spans_by_resource_and_scope`, `ExportTraceServiceRequest` |

All are MIT or Apache-2.0, on the `deny.toml` allow-list (line 29). The opentelemetry family stays at one version (0.33.1). `deny.toml` is not edited.

## Phase graph

```
TL2.1 deps ──┐
TL2.2 config ┴─► TL2.3 span exporter ─┐
TL2.4 span catalog ───────────────────┼─► TL2.5 subscriber ─┬─► TL2.8 HTTP and OTLP spans
                                      │                     ├─► TL2.9 query spans
                                      │                     ├─► TL2.10 alerts, deletion, retention spans
                                      │                     └─► TL2.11 eval, issues, backup spans
TL2.6 storage catalog and flush ─► TL2.7 compaction and GC spans
TL2.12 docs and specs   (after TL2.2 and TL2.4)
```

- TL2.1, TL2.2, TL2.4 and TL2.6 have disjoint files and can run in parallel.
- TL2.8 to TL2.11 have disjoint files and can run in parallel.
- TL2.6 and TL2.7 never touch server files.

| Phase | Files | Shared with other milestones |
|---|---|---|
| TL2.1 | `Cargo.toml`, `crates/server/Cargo.toml`, `Cargo.lock` | M5 (`backtrace` dependency), M6 |
| TL2.2 | `crates/server/src/config.rs`, `website/src/content/docs/docs/self-hosting/configuration-reference.md` | M3, M5, M6 (config keys, reference rows) |
| TL2.3 | `crates/server/src/observability/otlp.rs` | none |
| TL2.4 | `crates/server/src/observability/spans.rs` (new), `crates/server/src/observability/mod.rs` | M3, M5, M6 (`mod` lines) |
| TL2.5 | `crates/server/src/observability/subscriber.rs` | M3 (ring layer), M5 (crash layer) |
| TL2.6 | `crates/storage/src/spans.rs` (new), `crates/storage/src/lib.rs`, `crates/storage/src/flush.rs` | none |
| TL2.7 | `crates/storage/src/compaction/job.rs`, `crates/storage/src/gc.rs` | none |
| TL2.8 | `crates/server/src/http/mod.rs`, `crates/server/src/routes/otlp.rs`, `crates/server/src/ingest/grpc.rs` | `http/mod.rs`: M6 possibly |
| TL2.9 | `crates/server/src/routes/sql.rs`, `crates/server/src/mcp/mod.rs` | M6 possibly (`analytics::record` in handlers) |
| TL2.10 | `crates/server/src/alerts/evaluate.rs`, `crates/server/src/alerts/deliver.rs`, `crates/server/src/deletion/mod.rs`, `crates/server/src/retention.rs` | none known |
| TL2.11 | `crates/server/src/eval/dispatch.rs`, `crates/server/src/issues/worker.rs`, `crates/server/src/backup.rs` | none known |
| TL2.12 | `website/src/content/docs/docs/self-hosting/monitoring.md`, `CHANGELOG.md`, `docs/spec/12-operations.md`, `website/src/content/docs/docs/self-hosting/telemetry.md` | M3, M5, M6 (CHANGELOG, spec 12, telemetry page) |

Never two agents on the same file at once: `git commit -- <file>` commits the whole working-tree file, including another agent's half-done edit.

## Rules for every phase (the implementer is Haiku)

- Follow the plan exactly. Do not guess. If a step is unclear, contradicts the code, or cannot be done as written, stop and report the question instead of improvising.
- **If a name from M1 (or any existing name this plan cites) differs in the code, stop and report.** Do not adapt silently.
- Edit only the files of the phase's **Edit** list. Report any change needed elsewhere.
- **Do not run** `cargo build`, `cargo test`, `cargo clippy`, `cargo check`, or any `just` recipe: the machine is loaded, and the coordinator runs the checks once at the end of the milestone. These commands are fine:
  - read-only commands: `cargo info`, `grep`, reading sources under `~/.cargo/registry/src`;
  - `rustfmt --edition 2024 <your .rs files>`, which formats without building. Run it on every Rust file you changed before committing;
  - where a phase says so, `cargo metadata --format-version 1 --offline > /dev/null`, which only rewrites `Cargo.lock`.
- Write the tests the phase lists, even though you do not run them. Make them compile by reading the APIs: never guess a signature. Look it up in the code or the registry sources named in **Read first**.
- **Done when** for every phase means:
  - the listed files are changed as specified;
  - nothing else changed (`git status`);
  - the commit exists with the given subject.
- Commit only your phase's files: `git commit -m "<subject>" -- <file> <file> …`. Other agents work in the same checkout. If `.git/index.lock` exists, wait a few seconds and retry. For a new file, run `git add <file>` first.
- Edits to files that other milestones share are local: add or change only the lines this plan names. Never reorder or rewrite other code in them.
- Span values are ids, counts, fixed strings and durations only (D-TL-4).
  - Never record a URL, a query string, a header, a body, SQL text, a name, an email, an error's text or a user-supplied string.
  - Never use `#[instrument]`.
  - Build every span with its constructor from the catalog. Never write `info_span!` at a call site.
- Inside `tracing` macros, write no trailing comma after the last field.
- No `unwrap()` or `expect()` outside tests, unless a comment beside it states the invariant. No `unsafe`. Tests never call `std::env::set_var`.
- `mod` lines go where the phase says. `observability/mod.rs` is not fully sorted: M1's lines come first, then M4's.
- The milestone check, run by the coordinator after TL2.12 (§ Milestone check):
  1. `just fmt`;
  2. `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check`;
  3. `just deny`.

---

### TL2.1 — Dependencies

**Goal.** Add `tracing-opentelemetry` and the `trace` features of the OpenTelemetry API and SDK.

**Edit:**
- `Cargo.toml` (root)
- `crates/server/Cargo.toml`
- `Cargo.lock` (rewritten by `cargo metadata`)

**M1 names relied on.** These root entries, written by TL1.1:
- `opentelemetry = { version = "0.33.1", default-features = false, features = ["logs"] }`
- `opentelemetry_sdk = { version = "0.33.1", default-features = false, features = ["logs"] }`
- `opentelemetry-appender-tracing = { version = "0.33.1", default-features = false }`

The server's `opentelemetry-appender-tracing.workspace = true`.

**Steps.**
1. In the root `Cargo.toml` `[workspace.dependencies]`:
   - In the `opentelemetry` entry, change `features = ["logs"]` to `features = ["logs", "trace"]`. In its comment, change `` for `KeyValue` `` to `` for `KeyValue` and `TracerProvider` ``.
   - In the `opentelemetry_sdk` entry, change `features = ["logs"]` to `features = ["logs", "trace"]`. In its comment, change `Batch log processor, logger provider and resource` to `Batch log and span processors, logger and tracer providers, sampler and resource`.
   - Right after the `opentelemetry-appender-tracing` entry, add:
     ```toml
     # `tracing` spans to OTel spans (track TL M2, D-TL-2). No default features: `tracing-log` and
     # `metrics` are unused.
     tracing-opentelemetry = { version = "0.34.0", default-features = false }
     ```
2. In `crates/server/Cargo.toml` `[dependencies]`, right after `opentelemetry-appender-tracing.workspace = true`, add `tracing-opentelemetry.workspace = true`.
3. Run `cargo metadata --format-version 1 --offline > /dev/null` from the worktree root. Then check with `git diff Cargo.lock`:
   - the only new `[[package]]` is `tracing-opentelemetry` 0.34.0;
   - its dependency list is exactly `js-sys`, `opentelemetry`, `tracing`, `tracing-core`, `tracing-subscriber` and `web-time`. The two wasm-only crates appear because `Cargo.lock` lists target-specific dependencies; both are already in the lock;
   - `cetra-server`'s dependency list gained `tracing-opentelemetry`;
   - no other package changed version.

   If any check fails, stop and report. If `--offline` fails because the crate is not cached, run the command without `--offline` and report that.

**Tests.** None: a dependency declaration has no behaviour. The milestone's `just deny` checks the licences and the single versions.

**Done when.** The three files are changed as above and committed.

**Commit:** `Add tracing-opentelemetry and the SDK trace feature`

---

### TL2.2 — `traces` and `trace_sample_ratio`

**Goal.** `SelfMonitoringConfig` gains `traces` and a validated `trace_sample_ratio`, `OtlpEndpoint` gains `traces_url`, `exports()` covers traces, and the configuration reference lists both keys.

**Edit:**
- `crates/server/src/config.rs`
- `website/src/content/docs/docs/self-hosting/configuration-reference.md`

Both files are shared with M3, M5 and M6, which may already have added fields, validation, tests or rows. Every edit below is an addition or the change of a named line. Keep their additions. In particular:
- add the two fields among M1's fields, even if `SelfMonitoringConfig` or its `Default` already has another milestone's field;
- add the rows next to M1's rows, wherever other sections were added.

**M1 names relied on:**
- `SelfMonitoringConfig` with `otlp_endpoint`, `otlp_headers`, `logs`, `level`, `resource`, its `Default` impl, `logs_endpoint()` and `exports()`;
- `OtlpEndpoint { base, host }` with `logs_url()`;
- `default_self_monitoring_logs`, `default_self_monitoring_level`;
- the tests `self_monitoring_defaults`, `self_monitoring_loads_from_toml_and_env`, `otlp_endpoint_rules` and `unknown_self_monitoring_key_is_refused`;
- `load_from`.

**Steps.**
1. In `config.rs`, in `impl OtlpEndpoint`, add after `logs_url`:
   ```rust
       /// The URL spans are posted to: the base followed by `/v1/traces` (track TL M2).
       pub fn traces_url(&self) -> String {
           format!("{}/v1/traces", self.base)
       }
   ```
   In `OtlpEndpoint`'s doc comment, change `Its path is kept and `/v1/logs` is appended to it.` to ``Its path is kept and `/v1/logs` or `/v1/traces` is appended to it.``
2. Right before `/// `[self_monitoring]`…` (the `SelfMonitoringConfig` doc), add verbatim:
   ```rust
   /// `self_monitoring.trace_sample_ratio` (track TL M2): the probability that a root span is
   /// exported, from 0.0 to 1.0; children follow their root. Never NaN, so it is `Eq`.
   #[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
   #[serde(try_from = "f64")]
   pub struct TraceSampleRatio(f64);

   // `TryFrom` refuses NaN, the only `f64` that is not equal to itself, so equality is reflexive.
   impl Eq for TraceSampleRatio {}

   impl TraceSampleRatio {
       /// The default share of root spans, 10 % (D-TL-3).
       pub const DEFAULT: Self = Self(0.1);

       /// The ratio as a number in `[0.0, 1.0]`.
       pub fn get(self) -> f64 {
           self.0
       }
   }

   impl TryFrom<f64> for TraceSampleRatio {
       type Error = &'static str;

       /// Accepts `[0.0, 1.0]`; refuses NaN, infinities and anything outside. The reason never
       /// quotes the input.
       fn try_from(value: f64) -> Result<Self, Self::Error> {
           if (0.0..=1.0).contains(&value) {
               Ok(Self(value))
           } else {
               Err("must be a number from 0.0 to 1.0")
           }
       }
   }
   ```
3. In `SelfMonitoringConfig`:
   - Change its doc's first line to ``/// `[self_monitoring]`: export of cetra's own logs and spans to the operator's OTLP/HTTP endpoint``. Keep the rest of the doc.
   - Right after the `logs` field, add:
     ```rust
         /// Whether spans are exported. Defaults to `true` (track TL M2).
         #[serde(default = "default_self_monitoring_traces")]
         pub traces: bool,
     ```
   - Right after the `level` field, add:
     ```rust
         /// Probability that a root span is exported; its children follow it. Defaults to 0.1.
         #[serde(default = "default_trace_sample_ratio")]
         pub trace_sample_ratio: TraceSampleRatio,
     ```
   - In the `level` field's doc, append the sentence `Spans record the events that pass it.`
4. In `impl Default for SelfMonitoringConfig`, add `traces: default_self_monitoring_traces(),` after `logs: …` and `trace_sample_ratio: default_trace_sample_ratio(),` after `level: …`.
5. In `impl SelfMonitoringConfig`, add after `logs_endpoint`:
   ```rust
       /// The endpoint, when spans are exported: `otlp_endpoint` is set, `traces` is true and
       /// `trace_sample_ratio` is above 0. At 0 no span would be kept, so no span layer is built.
       pub fn traces_endpoint(&self) -> Option<&OtlpEndpoint> {
           self.otlp_endpoint
               .as_ref()
               .filter(|_| self.traces && self.trace_sample_ratio.get() > 0.0)
       }
   ```
   Replace the body of `exports` with `self.logs_endpoint().is_some() || self.traces_endpoint().is_some()`. Change its doc to `/// Whether anything is exported: logs, spans or both.`
6. After `fn default_self_monitoring_level`, add:
   ```rust
   fn default_self_monitoring_traces() -> bool {
       true
   }

   fn default_trace_sample_ratio() -> TraceSampleRatio {
       TraceSampleRatio::DEFAULT
   }
   ```
7. In `ServerConfig`, change the `self_monitoring` field's doc to ``/// `[self_monitoring]`: export of cetra's own logs and spans over OTLP/HTTP (track TL, D-TL-1).``
8. `ALIASES`, `is_filtered` and `is_secret_path` stay unchanged: the keys have only the split env form (`CETRA_SELF_MONITORING__TRACES`, `CETRA_SELF_MONITORING__TRACE_SAMPLE_RATIO`).
9. Change M1's test `unknown_self_monitoring_key_is_refused`:
   - its TOML becomes `[self_monitoring]\nmetrics = true\n`;
   - its comment, if any, says that cetra exports no metrics over OTLP (D-N-28). `traces` is now a key.
10. In M1's test `self_monitoring_defaults`, also assert:
    - `traces` is true;
    - `trace_sample_ratio.get()` is `0.1`;
    - `traces_endpoint()` is `None`.
11. In M1's test `self_monitoring_loads_from_toml_and_env` (config.rs, around line 2571), add the pair `("CETRA_SELF_MONITORING__TRACES", "false"),` to its `env` slice, right after `("CETRA_SELF_MONITORING__LOGS", "false"),`. Keep its assertions: with traces on by default, `exports()` would be true and `assert!(!monitoring.exports())` would fail. Also add `assert!(!monitoring.traces);` and `assert!(monitoring.traces_endpoint().is_none());` after `assert!(monitoring.logs_endpoint().is_none());`.
12. In `configuration-reference.md`, in the `## `[self_monitoring]`` section:
    - Intro sentence: change `Export cetra's own logs to an OTLP/HTTP endpoint you choose` to `Export cetra's own logs and traces to an OTLP/HTTP endpoint you choose`.
    - The `self_monitoring.otlp_endpoint` row: change `cetra posts protobuf, gzip-compressed, to the URL followed by `/v1/logs`` to ``cetra posts protobuf, gzip-compressed, to the URL followed by `/v1/logs` (logs) and `/v1/traces` (spans)``.
    - Right after the `self_monitoring.logs` row, add:
      ```markdown
      | `self_monitoring.traces` | | boolean | `true` | Export spans: HTTP requests, OTLP ingest, queries and background work. See [Monitoring](/docs/self-hosting/monitoring/#export-traces-over-otlp). |
      ```
    - The `self_monitoring.level` row: append ` Spans record the events that pass it.` to its Description.
    - Right after the `self_monitoring.level` row, add:
      ```markdown
      | `self_monitoring.trace_sample_ratio` | | number | `0.1` | The probability that a trace is exported, from `0.0` to `1.0`. Each root span is kept with this probability and its child spans follow it. `0.0` exports no span. |
      ```

**Tests** (in `config.rs` `mod tests`; add `TraceSampleRatio` to its `use super::{…}`):
- `trace_sample_ratio_bounds`: `TraceSampleRatio::try_from(v)` gives:
  - `Ok`, with `get()` equal to the input: `0.0`, `0.25`, `1.0`;
  - `Err("must be a number from 0.0 to 1.0")`: `-0.1`, `1.5`, `f64::NAN`, `f64::INFINITY`, `f64::NEG_INFINITY`.
- `trace_settings_load_from_toml_and_env`:
  - The TOML `[self_monitoring]\notlp_endpoint = "https://otlp.example.com/otlp"\ntraces = false\ntrace_sample_ratio = 0.5\n` gives:
    - `traces` false;
    - `trace_sample_ratio.get()` `0.5`;
    - `traces_endpoint()` `None`;
    - `exports()` true (logs).
  - The same endpoint with env `CETRA_SELF_MONITORING__TRACE_SAMPLE_RATIO=1` and `CETRA_SELF_MONITORING__LOGS=false` gives:
    - `trace_sample_ratio.get()` `1.0`: an integer is accepted;
    - `traces_endpoint()` `Some`, whose `traces_url()` is `https://otlp.example.com/otlp/v1/traces`;
    - `exports()` true.
- `a_trace_sample_ratio_out_of_range_is_refused`:
  - The TOML `trace_sample_ratio = 1.5` fails. Its error text contains `must be a number from 0.0 to 1.0` and `trace_sample_ratio`.
  - The env `CETRA_SELF_MONITORING__TRACE_SAMPLE_RATIO=-1` fails the same way.
  - The TOML `trace_sample_ratio = nan` fails, with the same reason in its text.
- `a_zero_ratio_exports_no_span`: with an endpoint, `logs = false` and `trace_sample_ratio = 0.0`:
  - `traces_endpoint()` is `None` and `exports()` is false;
  - with `logs` true, `exports()` is true.
- `traces_url_appends_v1_traces`: `parse_otlp_endpoint` on these inputs gives these `traces_url()`:
  - `http://collector:4318/` gives `http://collector:4318/v1/traces`;
  - `https://o1.ingest.de.sentry.io/api/2/integration/otlp` gives `…/integration/otlp/v1/traces`.
- `self_monitoring_defaults` and `unknown_self_monitoring_key_is_refused`, as changed in steps 9–10.
- `the_configuration_reference_matches_the_loader` (existing) must pass with the two new rows. The implementer checks by reading that each new field has a row.

**Done when.** The two files are changed as above and committed.

**Commit:** `Add the self_monitoring traces and trace_sample_ratio keys`

---

### TL2.3 — The OTLP/HTTP span exporter

**Goal.** `SpanPipeline` builds an env-free `SdkTracerProvider` around cetra's `OtlpSpanExporter`. The exporter shares M1's request path, refactored into `post`. The test receiver also accepts traces.

**Edit:**
- `crates/server/src/observability/otlp.rs`

M3 or M5 may already have edited this file. The `post` refactor (step 5) replaces only the body of `OtlpLogExporter::export`; every other edit is an addition. Keep other milestones' additions.

**M1 names relied on:**
- `EXPORT_FAILURE_TARGET`, `REQUEST_TIMEOUT`, `MAX_QUEUE_SIZE`, `SCHEDULED_DELAY`, `MAX_EXPORT_BATCH_SIZE`;
- `batch_config`, `resource`, `host_arch`, `os_type`, `request_headers`;
- `FailureLog::should_log`, `Failure { reason, status }`, `Failure::new`, `outcome`, `gzip`, `http_client`;
- `OtlpLogExporter { client, url, headers, resource, runtime, failures }` and its `LogExporter::export`;
- `LogPipeline`;
- `mod receiver` with `Received`, `Receiver { base, received, task }`, `ReceiverState { status, delay, received }`, `accept`, `Receiver::start`, `received`, `records`;
- the test helpers `exporter_for` and `test_record`.

**Steps.**
1. Add the imports below. Verify each path in the registry sources.
   - `std::sync::OnceLock`;
   - `opentelemetry::trace::TracerProvider as _`;
   - `opentelemetry_sdk::trace::{self as sdk_trace, SdkTracer, SdkTracerProvider, Sampler, SpanData, SpanExporter, SpanLimits}`;
   - `opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope`;
   - `opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest`.

   Keep M1's imports. The log-side `BatchConfig` and `BatchConfigBuilder` keep their names; the span side is always written `sdk_trace::BatchConfig` and `sdk_trace::BatchConfigBuilder`.
2. Extend the module doc with: `Spans (track TL M2) go the same way to `/v1/traces`, from a BatchSpanProcessor with an explicit ParentBased(TraceIdRatioBased) sampler.`
3. **One instance id per process.** In `resource`, replace `KeyValue::new("service.instance.id", uuid::Uuid::now_v7().to_string())` with `KeyValue::new("service.instance.id", service_instance_id())`. Add verbatim:
   ```rust
   /// `service.instance.id`: a UUIDv7 created on the first call and the same for the rest of the
   /// process, so the log and span resources match.
   fn service_instance_id() -> &'static str {
       static ID: OnceLock<String> = OnceLock::new();
       ID.get_or_init(|| uuid::Uuid::now_v7().to_string())
   }
   ```
   In `resource`'s doc comment, add: `The instance id is the process's, from [`service_instance_id`].`
4. **The shared request path.** Add verbatim, after `http_client`:
   ```rust
   /// Gzips `encoded` and posts it to `url` with `headers` on `runtime`; the future resolves to
   /// the outcome. The request is built and spawned before the future is returned: the SDK's
   /// batch thread has no runtime context, and reqwest creates its timeout timer in `send`. The
   /// response body is never read.
   fn post(
       client: &reqwest::Client,
       url: &str,
       headers: &HeaderMap,
       runtime: &Handle,
       encoded: &[u8],
   ) -> impl Future<Output = Result<(), Failure>> + Send + 'static {
       let spawned = gzip(encoded).map(|body| {
           let builder = client.post(url).headers(headers.clone()).body(body);
           runtime.spawn(async move { builder.send().await.map(|response| response.status()) })
       });
       async move {
           match spawned {
               Ok(handle) => outcome(handle.await),
               Err(_) => Err(Failure::new("encode", None)),
           }
       }
   }
   ```
   Under edition 2024, `impl Trait` in return position captures every lifetime of the arguments. The returned future owns everything it uses, so `'static` holds. If rustc nevertheless rejects the `'static` bound, write the return type `impl Future<Output = Result<(), Failure>> + Send + use<>`, and report it.
5. **The log exporter uses it.** Replace the body of `OtlpLogExporter`'s `export` with the code below. It does what M1's steps 1–5 did; the log line and the error are unchanged.
   ```rust
       fn export(&self, batch: LogBatch<'_>) -> impl Future<Output = OTelSdkResult> + Send {
           let records = batch.iter().count();
           let request = ExportLogsServiceRequest {
               resource_logs: group_logs_by_resource_and_scope(&batch, &self.resource),
           };
           let sent = post(
               &self.client,
               &self.url,
               &self.headers,
               &self.runtime,
               &request.encode_to_vec(),
           );
           let failures = Arc::clone(&self.failures);
           async move {
               match sent.await {
                   Ok(()) => Ok(()),
                   Err(failure) => {
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
           }
       }
   ```
   If M1's committed `export` signature differs (for example, if it names the batch differently), keep M1's signature and change only the body. If the body does something this replacement leaves out, stop and report.
6. **The span settings.** Add verbatim:
   ```rust
   /// The instrumentation scope of cetra's spans.
   const TRACER_NAME: &str = "cetra";

   /// Every setting given: `BatchConfigBuilder::default()` reads `OTEL_BSP_*`, and a value set
   /// here replaces the one it read. (`OTEL_BSP_EXPORT_TIMEOUT` and
   /// `OTEL_BSP_MAX_CONCURRENT_EXPORTS` are honoured only by the async-runtime processor, which
   /// cetra does not enable; [`REQUEST_TIMEOUT`] bounds an export.) The values are the logs'.
   fn span_batch_config() -> sdk_trace::BatchConfig {
       sdk_trace::BatchConfigBuilder::default()
           .with_max_queue_size(MAX_QUEUE_SIZE)
           .with_scheduled_delay(SCHEDULED_DELAY)
           .with_max_export_batch_size(MAX_EXPORT_BATCH_SIZE)
           .build()
   }

   /// The SDK's default limits, written out: its `Config::default()` reads `OTEL_SPAN_*_LIMIT`.
   fn span_limits() -> SpanLimits {
       SpanLimits {
           max_events_per_span: 128,
           max_attributes_per_span: 128,
           max_links_per_span: 128,
           max_attributes_per_event: 128,
           max_attributes_per_link: 128,
       }
   }

   /// A root span is kept with probability `ratio`; a child follows its parent (D-TL-3). Set
   /// explicitly: the SDK's default reads `OTEL_TRACES_SAMPLER`.
   fn sampler(ratio: f64) -> Sampler {
       Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio)))
   }
   ```
7. **The span exporter.** Add:
   ```rust
   /// Posts each batch to `<otlp_endpoint>/v1/traces` (track TL M2, D-TL-2).
   pub(crate) struct OtlpSpanExporter {
       client: reqwest::Client,
       url: String,
       headers: HeaderMap,
       resource: ResourceAttributesWithSchema,
       runtime: Handle,
       failures: Arc<FailureLog>,
   }
   ```
   1. Write a manual `impl fmt::Debug for OtlpSpanExporter`, the same as `OtlpLogExporter`'s: `url` and `header_names` only, with `finish_non_exhaustive()`.
   2. `impl SpanExporter for OtlpSpanExporter`:
      ```rust
          fn export(&self, batch: Vec<SpanData>) -> impl Future<Output = OTelSdkResult> + Send {
              let spans = batch.len();
              let request = ExportTraceServiceRequest {
                  resource_spans: group_spans_by_resource_and_scope(batch, &self.resource),
              };
              let sent = post(
                  &self.client,
                  &self.url,
                  &self.headers,
                  &self.runtime,
                  &request.encode_to_vec(),
              );
              let failures = Arc::clone(&self.failures);
              async move {
                  match sent.await {
                      Ok(()) => Ok(()),
                      Err(failure) => {
                          if failures.should_log(Instant::now()) {
                              tracing::warn!(
                                  target: EXPORT_FAILURE_TARGET,
                                  reason = failure.reason,
                                  status = failure.status,
                                  spans,
                                  "self-monitoring span export failed"
                              );
                          }
                          Err(OTelSdkError::InternalFailure(failure.reason.to_owned()))
                      }
                  }
              }
          }

          fn set_resource(&mut self, resource: &Resource) {
              self.resource = resource.into();
          }
      ```
      `shutdown_with_timeout` and `force_flush` keep their defaults.
8. **The pipeline.** Add:
   ```rust
   /// The provider of the self-monitoring span export. The subscriber's span layer creates its
   /// spans with [`SpanPipeline::tracer`]; the subscriber's guard shuts it down at exit.
   pub(crate) struct SpanPipeline {
       provider: SdkTracerProvider,
       host: String,
   }

   impl SpanPipeline {
       /// Builds the exporter, its batch processor, the sampler and the provider. No `OTEL_*`
       /// variable is read.
       pub(crate) fn new(
           config: &SelfMonitoringConfig,
           endpoint: &OtlpEndpoint,
           runtime: Handle,
       ) -> Result<Self, ObservabilityError>;

       /// The tracer of cetra's spans (scope [`TRACER_NAME`]).
       pub(crate) fn tracer(&self) -> SdkTracer;

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
      5. Build `OtlpSpanExporter { client, url: endpoint.traces_url(), headers, resource: (&resource).into(), runtime, failures: Arc::new(FailureLog::default()) }`.
      6. `let processor = sdk_trace::BatchSpanProcessor::builder(exporter).with_batch_config(span_batch_config()).build();`
         - **Never** use `TracerProviderBuilder::with_batch_exporter`: it uses the env-reading default config.
      7. Build the provider:
         ```rust
         let provider = SdkTracerProvider::builder()
             .with_sampler(sampler(config.trace_sample_ratio.get()))
             .with_span_limits(span_limits())
             .with_resource(resource)
             .with_span_processor(processor)
             .build();
         ```
         `builder()` evaluates the env-reading `Config::default()` (sampler, limits, resource). `with_sampler`, `with_span_limits` and `with_resource` replace those three. The id generator default reads nothing.
      8. Return `SpanPipeline { provider, host: endpoint.host().to_owned() }`.
   2. `tracer` returns `self.provider.tracer(TRACER_NAME)`.
   3. `host` returns `&self.host`.
   4. `shutdown` returns `self.provider.shutdown_with_timeout(timeout)`.
   5. Write a manual `Debug` for `SpanPipeline` that prints `host` only, with `finish_non_exhaustive()`.
9. **The receiver accepts traces.** In `#[cfg(test)] pub(crate) mod receiver`:
   1. Add `traces: Arc<Mutex<Vec<(HeaderMap, ExportTraceServiceRequest)>>>` to `Receiver` and to `ReceiverState`. Fill both from one `Arc` in `start`, as for `received`.
   2. In `start`, add `.route("/otlp/v1/traces", post(accept_traces))` to the router, next to the logs route.
   3. Move the body reading of `accept` into a function, and have `accept` call it:
      ```rust
      /// The request body, gunzipped when `content-encoding` is `gzip`. `None` when it does not
      /// gunzip.
      fn body_bytes(headers: &HeaderMap, body: &Bytes) -> Option<Vec<u8>>
      ```
   4. Add the traces handler: `async fn accept_traces(State(state): State<Arc<ReceiverState>>, headers: HeaderMap, body: Bytes) -> StatusCode`. It does this, in order:
      1. sleeps `state.delay`;
      2. calls `body_bytes`;
      3. decodes with `ExportTraceServiceRequest::decode`, pushes `(headers, request)` onto `state.traces`, and answers `state.status`.

      A body that does not gunzip or decode answers 400 and pushes nothing.
   5. Add two methods to `Receiver`:
      - `pub(crate) fn trace_requests(&self) -> Vec<ExportTraceServiceRequest>`: the requests so far, in arrival order;
      - `pub(crate) fn trace_headers(&self) -> Vec<HeaderMap>`: their headers, in the same order.
   6. Add, verbatim apart from the imports. The `receiver` module has its own `use` list (no `use super::*`), so add to it `opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest` (also needed by steps 9.1–9.5), `opentelemetry_proto::tonic::trace::v1::Span as ProtoSpan`, `opentelemetry_proto::tonic::common::v1::{KeyValue as ProtoKeyValue, any_value::Value as ProtoValue}`, `std::collections::BTreeSet`):
      ```rust
      /// The span requests a test captured.
      #[derive(Debug, Clone, Default)]
      pub(crate) struct Captured {
          pub(crate) requests: Vec<ExportTraceServiceRequest>,
      }

      impl Captured {
          /// Every span of every request, in order.
          pub(crate) fn spans(&self) -> Vec<ProtoSpan> {
              self.requests
                  .iter()
                  .flat_map(|request| request.resource_spans.iter())
                  .flat_map(|resource| resource.scope_spans.iter())
                  .flat_map(|scope| scope.spans.iter().cloned())
                  .collect()
          }

          /// The spans named `name`.
          pub(crate) fn named(&self, name: &str) -> Vec<ProtoSpan> {
              self.spans()
                  .into_iter()
                  .filter(|span| span.name == name)
                  .collect()
          }

          /// Everything the requests hold, as `Debug` text, for leak checks.
          pub(crate) fn text(&self) -> String {
              format!("{:?}", self.requests)
          }
      }

      /// The string value of `key`, if it has one.
      pub(crate) fn string_attribute(attributes: &[ProtoKeyValue], key: &str) -> Option<String> {
          attributes
              .iter()
              .find(|attribute| attribute.key == key)
              .and_then(|attribute| attribute.value.as_ref())
              .and_then(|value| match &value.value {
                  Some(ProtoValue::StringValue(text)) => Some(text.clone()),
                  _ => None,
              })
      }

      /// The integer value of `key`, if it has one.
      pub(crate) fn int_attribute(attributes: &[ProtoKeyValue], key: &str) -> Option<i64> {
          attributes
              .iter()
              .find(|attribute| attribute.key == key)
              .and_then(|attribute| attribute.value.as_ref())
              .and_then(|value| match &value.value {
                  Some(ProtoValue::IntValue(number)) => Some(*number),
                  _ => None,
              })
      }

      /// The keys of `attributes`.
      pub(crate) fn attribute_keys(attributes: &[ProtoKeyValue]) -> BTreeSet<String> {
          attributes
              .iter()
              .map(|attribute| attribute.key.clone())
              .collect()
      }
      ```
   7. `otlp.rs`'s own `mod tests` already has private `string_value`, `string_attr` and `int_attr` over the same types. Leave them and their call sites unchanged. The new `receiver` helpers are `pub(crate)` for the tests of other modules (TL2.5, TL2.8–TL2.11). New tests in this file may use either.
10. Add a test helper `fn span_exporter_for(receiver_base: &str) -> OtlpSpanExporter` in `mod tests`. It builds the exporter as `SpanPipeline::new` does, from `http_client()`, `request_headers(None)`, `resource(vec![])` and `Handle::current()`, with `url` = `parse_otlp_endpoint(receiver_base).unwrap().traces_url()`.

**Tests** (in `otlp.rs` `mod tests`; tests that block use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`):
- `spans_arrive_with_resource_scope_and_headers`:
  1. Start a `Receiver` with 200 and no delay.
  2. Build a `SelfMonitoringConfig` with:
     - `otlp_endpoint: Some(parse_otlp_endpoint(&receiver.base).unwrap())`;
     - `otlp_headers: Some(Secret::new("authorization=Bearer t0k3n".to_owned()))`;
     - `resource: "deployment.environment=test".to_owned()`;
     - `trace_sample_ratio: TraceSampleRatio::try_from(1.0).unwrap()`;
     - the rest default.
  3. Call `SpanPipeline::new(…, Handle::current())`.
  4. With `opentelemetry::trace::Tracer` in scope, run `pipeline.tracer().in_span("direct", |_| {})`.
  5. Shut down through `spawn_blocking` with 2 s, and assert the inner result `.is_ok()`.
  6. Assert:
     - one span named `direct` arrived;
     - the first entry of `receiver.trace_headers()` has `authorization` `Bearer t0k3n`, `content-encoding` `gzip`, `content-type` `application/x-protobuf` and `user-agent` `cetra/<CARGO_PKG_VERSION>`;
     - the resource has `service.name` `cetra` and `deployment.environment` `test`;
     - the scope name is `cetra`.
- `logs_and_traces_share_the_resource`: two calls of `resource(vec![])` give the same `service.instance.id` value (`Resource::get(&Key::new("service.instance.id"))`).
- `span_export_reports_an_error_status_as_a_failure`:
  1. Start a `Receiver` with 500 and use `span_exporter_for`.
  2. `let result = exporter.export(Vec::new()).await;`. An empty batch still posts a request.
  3. Assert `matches!(result, Err(OTelSdkError::InternalFailure(ref reason)) if reason == "status")`.
- `span_export_reports_a_closed_port_as_a_failure`: the same with `span_exporter_for("http://127.0.0.1:1/otlp")`. The reason is `"connect"`.
- `the_log_exporter_still_reports_failures_through_post`: M1's `export_reports_an_error_status_as_a_failure` and `export_reports_a_closed_port_as_a_failure` stay unchanged and must still hold. They now exercise `post`. Nothing to write; do not delete them.
- `sampler_is_parent_based_on_the_ratio`: `format!("{:?}", sampler(0.25))` contains `ParentBased` and `TraceIdRatioBased(0.25)`.
- `span_batch_config_sets_every_field`: `format!("{:?}", span_batch_config())` contains `max_queue_size: 2048`, `scheduled_delay: 1s` and `max_export_batch_size: 512`.
- `span_limits_are_written_out`: every field of `span_limits()` is 128.
- `span_debug_hides_the_header_values`: `format!("{:?}", exporter)` of an `OtlpSpanExporter` whose headers include `authorization: Bearer t0k3n` contains `authorization` and not `t0k3n`. `format!("{:?}", pipeline)` contains the host only.
- `otel_environment_variables_change_nothing_for_spans` (parent) and `env_child_exports_three_spans` (child), built like M1's env test:
  - The child is `#[ignore = "run by otel_environment_variables_change_nothing_for_spans in a child process"]`. It returns at once when `CETRA_TEST_OTLP_CHILD_ENDPOINT` is unset. Otherwise it:
    1. builds a `SpanPipeline` to that endpoint, with `otlp_headers` `x-cetra-test=1` and a ratio of 1.0;
    2. runs `tracer.in_span("child-1", |cx| cx.span().set_attribute(KeyValue::new("n", 1_i64)))`, and the same for `child-2` and `child-3` (trait `opentelemetry::trace::TraceContextExt` for `cx.span()`);
    3. shuts down through `spawn_blocking` with 2 s.
  - The parent:
    1. starts a `Receiver`.
    2. In `spawn_blocking`, runs `std::process::Command::new(std::env::current_exe()?)` with:
       - the args `observability::otlp::tests::env_child_exports_three_spans`, `--exact`, `--ignored`, `--nocapture`, `--test-threads=1`;
       - the env `CETRA_TEST_OTLP_CHILD_ENDPOINT=<receiver.base>`;
       - and `OTEL_TRACES_SAMPLER=always_off`, `OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT=0`, `OTEL_EXPORTER_OTLP_TRACES_HEADERS=x-leaked=env`, `OTEL_EXPORTER_OTLP_HEADERS=x-leaked-all=env`, `OTEL_RESOURCE_ATTRIBUTES=leaked.attr=1`, `OTEL_SERVICE_NAME=leaked-service`, `OTEL_BSP_MAX_QUEUE_SIZE=1`, `OTEL_BSP_MAX_EXPORT_BATCH_SIZE=1`, `OTEL_BSP_SCHEDULE_DELAY=600000`;
       - then `.output()`.
    3. Asserts:
       - the child exited successfully and its stdout contains `1 passed`;
       - three spans arrived: neither the sampler (`always_off`) nor the queue size (1) was taken from the environment. `OTEL_BSP_SCHEDULE_DELAY` cannot be detected this way, because shutdown flushes whatever the delay. It is set anyway, as in M1's test, and `span_batch_config_sets_every_field` checks the delay;
       - each has the integer attribute `n` 1: the attribute limit was not taken from the environment;
       - the resource has `service.name` `cetra` and no `leaked.attr`;
       - every entry of `trace_headers()` has `x-cetra-test` and `content-encoding: gzip`, and none has `x-leaked` or `x-leaked-all`.

**Done when.** The file is changed as above and committed.

**Commit:** `Add the self-monitoring OTLP span exporter`

---

### TL2.4 — The server's span catalog

**Goal.** One file defines every server span: its name, its constant attribute keys and the constructor that builds it. A golden test fixes the list.

**Edit:**
- `crates/server/src/observability/spans.rs` (new)
- `crates/server/src/observability/mod.rs` (one `mod` line)

**M1 names relied on:** `observability/mod.rs` exists (TL1.3).

**Steps.**
1. In `observability/mod.rs`:
   - Insert `pub(crate) mod spans;` between `pub mod panic;` and `pub mod subscriber;`, whatever other milestones added elsewhere in the list.
   - The module doc is one sentence ending `installs.`. Change `the OTLP log export to the operator's endpoint, and the `tracing` subscriber the CLI installs.` to `the OTLP log and span export to the operator's endpoint, the server's spans, and the `tracing` subscriber the CLI installs.`
   - If another milestone already reworded the doc, add only `the server's spans` and keep its wording.
2. Create `spans.rs` with exactly this content, doc comments included:
   ```rust
   //! The server's spans (track TL M2, D-TL-4). Every span is built here, by one constructor,
   //! so the names and attribute keys are constants and reviewable in one place. Values are ids,
   //! counts, fixed strings and durations: never a URL, a query string, a body, SQL text, a name,
   //! an email or an error's text. A field declared `Empty` is recorded later with
   //! `span.record(KEY, value)`; counts go through [`count`], because `tracing-opentelemetry`
   //! records a `u64` as a string. Spans cost little when no layer records them (no endpoint).

   use cetra_core::ids::ProjectId;
   use tracing::Span;
   use tracing::field::Empty;
   use uuid::Uuid;

   /// `otel.kind`: the OTel span kind (`tracing-opentelemetry` special field).
   pub(crate) const OTEL_KIND: &str = "otel.kind";
   /// `otel.status_code`: `"error"` marks the span failed (`tracing-opentelemetry` special field).
   pub(crate) const OTEL_STATUS: &str = "otel.status_code";
   /// The request method, `GET` … `OPTIONS` or `other` (`telemetry::Method`).
   pub(crate) const METHOD: &str = "http.request.method";
   /// The matched route template (D150), or `<unmatched>`.
   pub(crate) const ROUTE: &str = "http.route";
   /// The response status, an integer.
   pub(crate) const STATUS_CODE: &str = "http.response.status_code";
   /// `http` or `grpc`.
   pub(crate) const PROTOCOL: &str = "protocol";
   pub(crate) const PROJECT_ID: &str = "project_id";
   /// Spans accepted by an export.
   pub(crate) const SPANS: &str = "spans";
   /// Spans rejected by an export.
   pub(crate) const REJECTED: &str = "rejected";
   pub(crate) const BYTES: &str = "bytes";
   /// A fixed word per span: `ok` or why not.
   pub(crate) const OUTCOME: &str = "outcome";
   pub(crate) const KIND: &str = "kind";
   /// An MCP tool name, from the tool list (never the caller's string).
   pub(crate) const TOOL: &str = "tool";
   pub(crate) const ROWS: &str = "rows";
   pub(crate) const JOB_ID: &str = "job_id";
   pub(crate) const EVALUATOR_ID: &str = "evaluator_id";
   pub(crate) const RULE_ID: &str = "rule_id";
   pub(crate) const DELIVERY_ID: &str = "delivery_id";
   pub(crate) const CHANNEL_ID: &str = "channel_id";
   pub(crate) const ATTEMPT: &str = "attempt";
   pub(crate) const DELETION_ID: &str = "deletion_id";
   pub(crate) const PROJECTS: &str = "projects";
   pub(crate) const FILES: &str = "files";
   pub(crate) const FAILURES: &str = "failures";
   pub(crate) const CANDIDATE_ID: &str = "candidate_id";
   pub(crate) const SOURCE_ID: &str = "source_id";
   pub(crate) const MODE: &str = "mode";
   pub(crate) const OBJECTS: &str = "objects";

   /// [`PROTOCOL`] of an OTLP/HTTP export.
   pub(crate) const PROTOCOL_HTTP: &str = "http";
   /// [`PROTOCOL`] of an OTLP/gRPC export.
   pub(crate) const PROTOCOL_GRPC: &str = "grpc";
   /// [`KIND`] of a SQL console query answered as JSON.
   pub(crate) const QUERY_SQL: &str = "sql";
   /// [`KIND`] of a SQL console export (the span covers its start, to the first rows).
   pub(crate) const QUERY_SQL_EXPORT: &str = "sql_export";
   /// [`KIND`] of an MCP tool call.
   pub(crate) const QUERY_MCP: &str = "mcp";

   /// `value` as an `i64` attribute, `i64::MAX` when it does not fit.
   pub(crate) fn count<T: TryInto<i64>>(value: T) -> i64 {
       value.try_into().unwrap_or(i64::MAX)
   }

   /// An eval job's `kind` column as a fixed word: `rule`, `manual`, `backfill`, else `other`.
   pub(crate) fn job_kind(kind: &str) -> &'static str {
       match kind {
           "rule" => "rule",
           "manual" => "manual",
           "backfill" => "backfill",
           _ => "other",
       }
   }

   /// One HTTP request, from the request log middleware (`http::log_request`).
   pub(crate) fn http_request(method: &axum::http::Method, route: &str) -> Span {
       tracing::info_span!(
           "http.request",
           { OTEL_KIND } = "server",
           { METHOD } = ?crate::telemetry::Method::of(method),
           { ROUTE } = route,
           { STATUS_CODE } = Empty,
           { OTEL_STATUS } = Empty
       )
   }

   /// One OTLP trace export (`protocol` is [`PROTOCOL_HTTP`] or [`PROTOCOL_GRPC`]). Over gRPC it
   /// is the request's root, so its kind is `server`; over HTTP it is a child of `http.request`.
   pub(crate) fn otlp_export(protocol: &'static str) -> Span {
       let kind = if protocol == PROTOCOL_GRPC {
           "server"
       } else {
           "internal"
       };
       tracing::info_span!(
           "otlp.export",
           { OTEL_KIND } = kind,
           { PROTOCOL } = protocol,
           { PROJECT_ID } = Empty,
           { SPANS } = Empty,
           { REJECTED } = Empty,
           { BYTES } = Empty,
           { OUTCOME } = Empty
       )
   }

   /// One query of `kind` ([`QUERY_SQL`], [`QUERY_SQL_EXPORT`], [`QUERY_MCP`]), child of
   /// `parent`. Never carries the SQL text.
   pub(crate) fn query(parent: &Span, kind: &'static str) -> Span {
       tracing::info_span!(
           parent: parent,
           "query",
           { KIND } = kind,
           { PROJECT_ID } = Empty,
           { TOOL } = Empty,
           { ROWS } = Empty,
           { OUTCOME } = Empty
       )
   }

   /// One eval job run (`eval::dispatch::run_claimed`).
   pub(crate) fn eval_job(job_id: Uuid, project: ProjectId, evaluator_id: Uuid, kind: &str) -> Span {
       tracing::info_span!(
           "eval.job",
           { JOB_ID } = %job_id,
           { PROJECT_ID } = %project,
           { EVALUATOR_ID } = %evaluator_id,
           { KIND } = job_kind(kind),
           { OUTCOME } = Empty
       )
   }

   /// One alert rule's evaluation (`alerts::evaluate::evaluate_rule`).
   pub(crate) fn alert_evaluate(rule_id: Uuid, project: ProjectId) -> Span {
       tracing::info_span!(
           "alerts.evaluate",
           { RULE_ID } = %rule_id,
           { PROJECT_ID } = %project,
           { OUTCOME } = Empty
       )
   }

   /// One alert delivery attempt (`alerts::deliver::deliver_one`). `kind` is the channel kind.
   pub(crate) fn alert_deliver(
       delivery_id: Uuid,
       channel_id: Uuid,
       kind: &'static str,
       attempt: i64,
   ) -> Span {
       tracing::info_span!(
           "alerts.deliver",
           { DELIVERY_ID } = %delivery_id,
           { CHANNEL_ID } = %channel_id,
           { KIND } = kind,
           { ATTEMPT } = attempt,
           { OUTCOME } = Empty
       )
   }

   /// One step of one deletion row (`deletion::run_once`). `kind` is the row's kind.
   pub(crate) fn deletion_apply(deletion_id: Uuid, kind: &'static str) -> Span {
       tracing::info_span!(
           "deletion.apply",
           { DELETION_ID } = %deletion_id,
           { KIND } = kind,
           { OUTCOME } = Empty
       )
   }

   /// One retention pass (`retention::retention_once`), hourly.
   pub(crate) fn retention_pass() -> Span {
       tracing::info_span!(
           "retention.pass",
           { PROJECTS } = Empty,
           { FILES } = Empty,
           { FAILURES } = Empty,
           { OUTCOME } = Empty
       )
   }

   /// One issue candidate run (`issues::worker::run_candidate`).
   pub(crate) fn issues_candidate(candidate_id: Uuid, source_id: Uuid) -> Span {
       tracing::info_span!(
           "issues.candidate",
           { CANDIDATE_ID } = %candidate_id,
           { SOURCE_ID } = %source_id,
           { OUTCOME } = Empty
       )
   }

   /// One online backup (`backup::run`).
   pub(crate) fn backup() -> Span {
       tracing::info_span!(
           "backup",
           { MODE } = Empty,
           { OBJECTS } = Empty,
           { BYTES } = Empty,
           { OUTCOME } = Empty
       )
   }
   ```
   If `crate::telemetry::Method::of` does not exist with the signature `pub(crate) fn of(method: &axum::http::Method) -> Self`, or `Method` does not derive `Debug`, stop and report.

**Tests** (in `spans.rs`, `#[cfg(test)] mod tests`). Use this helper. A span built while no subscriber enables it is disabled, but it keeps its metadata (`Span::new_disabled`):
```rust
fn shape(span: &Span) -> (&'static str, Vec<&'static str>) {
    let meta = span.metadata().expect("a span keeps its metadata");
    (meta.name(), meta.fields().iter().map(|field| field.name()).collect())
}
```
- `the_span_catalog_is_fixed`: builds every span with fixed arguments:
  - `Uuid::nil()` for ids;
  - `"018f6b4e-7c1a-7c1a-8c1a-000000000001".parse::<ProjectId>().unwrap()` for projects;
  - `axum::http::Method::GET`;
  - `"/x"` as the route;
  - `&Span::none()` as the query parent.

  It asserts that the vector of `shape`s equals exactly:
  ```
  ("http.request", ["otel.kind", "http.request.method", "http.route", "http.response.status_code", "otel.status_code"])
  ("otlp.export", ["otel.kind", "protocol", "project_id", "spans", "rejected", "bytes", "outcome"])
  ("query", ["kind", "project_id", "tool", "rows", "outcome"])
  ("eval.job", ["job_id", "project_id", "evaluator_id", "kind", "outcome"])
  ("alerts.evaluate", ["rule_id", "project_id", "outcome"])
  ("alerts.deliver", ["delivery_id", "channel_id", "kind", "attempt", "outcome"])
  ("deletion.apply", ["deletion_id", "kind", "outcome"])
  ("retention.pass", ["projects", "files", "failures", "outcome"])
  ("issues.candidate", ["candidate_id", "source_id", "outcome"])
  ("backup", ["mode", "objects", "bytes", "outcome"])
  ```
  A new span or key must change this list, which shows in review (D-TL-4).
- `spans_have_this_module_as_target`: the target of each span is `cetra_server::observability::spans` (`span.metadata().unwrap().target()`). The span layer's filter keeps `cetra_` targets (TL2.5).
- `spans_are_info`: each span's level is `tracing::Level::INFO`.
- `count_saturates`: `count(3_usize)` is 3, `count(u64::MAX)` is `i64::MAX`, `count(7_u16)` is 7.
- `job_kind_maps_unknown_values_to_other`: `rule`, `manual` and `backfill` map to themselves; `""` and `Rule` map to `other`.

**Done when.** The two files are created or changed as above and committed.

**Commit:** `Add the server's span catalog`

---

### TL2.5 — The span layer in the subscriber

**Goal.**
- `build` adds a `tracing-opentelemetry` layer with its own filter when traces are exported.
- The stdout layer and M1's log layer no longer see spans, so stdout lines are unchanged.
- `Observability` flushes both exports in parallel.
- A test helper captures exported spans.

**Edit:**
- `crates/server/src/observability/subscriber.rs`

M3 (ring-buffer layer) and M5 (crash layer) may already have edited `build`, `Observability`, `shutdown` and `init`. Every M2 edit below is an addition, or the replacement of one named M1 statement:
- the stdout `match`;
- two expressions in the logs block;
- the `Observability` literal (add `traces`);
- the `shutdown` body: keep their steps after the new lines.

Keep every layer push, field, flush step and test of theirs.

**M1 names relied on:**
- `build`, `init`, `Observability { logs }`, `Observability::shutdown`, `exporting_logs`;
- `FLUSH_TIMEOUT`, `BoxedLayer`, `ComposedSubscriber`;
- `export_filter`, `export_target_allowed`, `log_filter`;
- `EXPORT_FAILURE_TARGET`, `LogPipeline`;
- the tests' `Buffer` writer and `stdout_json_matches_the_former_builder`.

TL2.3's `SpanPipeline` and `receiver::{Receiver, Captured}` are committed.

**Steps.**
1. Imports to add:
   - `tracing_opentelemetry::OpenTelemetryLayer`;
   - `opentelemetry_sdk::trace::SdkTracer`;
   - `opentelemetry_sdk::error::OTelSdkResult`;
   - `super::otlp::SpanPipeline` (beside `LogPipeline`);
   - `tracing_subscriber::filter::LevelFilter`, added to the existing `tracing_subscriber::filter::{…}` list.

   Verify each path in the registry sources.
2. Module doc: replace sentences 2 and 3, from `The stdout layer` up to and including `minus the exporter's own stack.`, with this text. If another milestone added a sentence about its own layer, keep that sentence. `The stdout layer is `log.level` with `rmcp` capped; it sees events only, so the spans never change its lines. The OTLP log layer is `self_monitoring.level` with the same cap, minus the exporter's own stack. The OTLP span layer exports cetra's own spans (track TL M2) with the events the log layer would export.`
3. Add verbatim, after `export_target_allowed`:
   ```rust
   /// Target prefix of cetra's own crates.
   const CETRA_TARGET_PREFIX: &str = "cetra_";

   /// Whether a span of `target` at `level` is exported: cetra's own, at `info` or above. Spans
   /// of other crates never are: rmcp's carry whole MCP requests (D-TL-4).
   fn span_target_allowed(target: &str, level: Level) -> bool {
       target.starts_with(CETRA_TARGET_PREFIX) && level <= Level::INFO
   }

   /// The span layer of `tracer`. It sees cetra's spans and the events that `events` (the
   /// export filter of `self_monitoring.level`) and [`export_target_allowed`] pass, so a span
   /// records what the log export carries (D-TL-3). An ERROR event inside a span sets its status
   /// to error (`OpenTelemetryLayer::on_event`, whatever the settings). Everything else the layer
   /// can add is off: code location, thread, target, level and busy/idle attributes, OTel context
   /// activation (a parent is the enclosing `tracing` span this layer sees, or an explicit
   /// `parent:`), and the exception mappings, which would copy error text into span attributes
   /// and statuses. No propagator exists, so no incoming `traceparent` is read (D-TL-7).
   fn span_layer(tracer: SdkTracer, events: EnvFilter) -> BoxedLayer {
       let spans = filter_fn(|metadata| {
           metadata.is_span() && span_target_allowed(metadata.target(), *metadata.level())
       });
       let events = FilterExt::<Registry>::and(
           events,
           filter_fn(|metadata| {
               metadata.is_event() && export_target_allowed(metadata.target(), *metadata.level())
           }),
       );
       OpenTelemetryLayer::<Registry, SdkTracer>::new(tracer)
           .with_location(false)
           .with_threads(false)
           .with_target(false)
           .with_level(false)
           .with_tracked_inactivity(false)
           .with_context_activation(false)
           .with_error_fields_to_exceptions(false)
           .with_error_records_to_exceptions(false)
           .with_error_events_to_exceptions(false)
           .with_error_events_to_status(false)
           .with_filter(FilterExt::<Registry>::or(spans, events))
           .boxed()
   }
   ```
4. In `build`, make these four local edits:
   1. Replace the `let stdout: BoxedLayer = match format { … };` statement with the code below. The `stdout_filter` line above it stays.
      ```rust
          // Events only: a span this layer does not see is never printed (json's `span` and
          // `spans` keys, pretty's `in` lines), so stdout lines are the same with spans (track TL
          // M2). The json span options are off as well.
          // The hint keeps `log.level`'s: `And` takes the more verbose of its two filters'
          // hints, and a `filter_fn` without one would count as TRACE, so every disabled
          // `debug!` and `trace!` callsite would be evaluated again.
          let stdout_events =
              filter_fn(|metadata| metadata.is_event()).with_max_level_hint(LevelFilter::TRACE);
          let stdout: BoxedLayer = match format {
              LogFormat::Pretty => tracing_subscriber::fmt::layer()
                  .pretty()
                  .with_writer(writer)
                  .with_filter(FilterExt::<Registry>::and(stdout_filter, stdout_events))
                  .boxed(),
              LogFormat::Json => tracing_subscriber::fmt::layer()
                  .json()
                  .with_current_span(false)
                  .with_span_list(false)
                  .with_writer(writer)
                  .with_filter(FilterExt::<Registry>::and(stdout_filter, stdout_events))
                  .boxed(),
          };
      ```
   2. In the logs block, change `LogPipeline::new(&config.self_monitoring, endpoint, runtime)?` to `LogPipeline::new(&config.self_monitoring, endpoint, runtime.clone())?`.
   3. In the logs block, change the `targets` closure body from `export_target_allowed(metadata.target(), *metadata.level())` to `metadata.is_event() && export_target_allowed(metadata.target(), *metadata.level())`. The log bridge ignores spans; without this, `self_monitoring.level = "info"` would enable every `info` span even with traces off.
   4. Right after the logs block (`let logs = match … };`), add:
      ```rust
          let traces = match config.self_monitoring.traces_endpoint() {
              Some(endpoint) => {
                  let events = export_filter(&config.self_monitoring.level)
                      .map_err(ObservabilityError::ExportLevel)?;
                  let pipeline =
                      SpanPipeline::new(&config.self_monitoring, endpoint, runtime.clone())?;
                  layers.push(span_layer(pipeline.tracer(), events));
                  Some(pipeline)
              }
              None => None,
          };
      ```
      Then change the final `Observability { logs }` to `Observability { logs, traces }`. If other milestones' layers or fields are already in `build` or in the literal, keep them and only add `traces`.
5. `Observability`:
   - Add the field `traces: Option<SpanPipeline>,` after `logs`.
   - Add the method:
     ```rust
         /// Whether spans are exported.
         pub fn exporting_traces(&self) -> bool {
             self.traces.is_some()
         }
     ```
   - Replace the body of `shutdown` with the code below. If another milestone added a step to it, keep that step after these lines.
     ```rust
         pub async fn shutdown(self) {
             let logs = self
                 .logs
                 .map(|pipeline| tokio::task::spawn_blocking(move || pipeline.shutdown(FLUSH_TIMEOUT)));
             let traces = self
                 .traces
                 .map(|pipeline| tokio::task::spawn_blocking(move || pipeline.shutdown(FLUSH_TIMEOUT)));
             let (logs_flushed, traces_flushed) = tokio::join!(flushed(logs), flushed(traces));
             if !logs_flushed {
                 tracing::warn!(
                     target: EXPORT_FAILURE_TARGET,
                     "self-monitoring: the log export did not flush within 2 s"
                 );
             }
             if !traces_flushed {
                 tracing::warn!(
                     target: EXPORT_FAILURE_TARGET,
                     "self-monitoring: the span export did not flush within 2 s"
                 );
             }
         }
     ```
   - Change its doc to `Flushes and stops both exports in parallel on blocking threads, waiting at most [`FLUSH_TIMEOUT`] (plus 500 ms for the hand-off). Never fails: a flush that errs or times out writes one `warn` line under [`EXPORT_FAILURE_TARGET`] (stdout only).`
   - Add, after `impl Observability`:
     ```rust
     /// Whether a flush started on a blocking thread succeeded within [`FLUSH_TIMEOUT`] plus
     /// 500 ms. No flush counts as success. The SDK's error is not logged: it can hold `Debug`
     /// of internals.
     async fn flushed(flush: Option<tokio::task::JoinHandle<OTelSdkResult>>) -> bool {
         let Some(flush) = flush else {
             return true;
         };
         matches!(
             tokio::time::timeout(FLUSH_TIMEOUT + Duration::from_millis(500), flush).await,
             Ok(Ok(Ok(())))
         )
     }
     ```
6. In `init`, right after M1's `if let Some(pipeline) = &observability.logs { … }` line, add:
   ```rust
       if let Some(pipeline) = &observability.traces {
           tracing::info!(
               host = pipeline.host(),
               ratio = config.self_monitoring.trace_sample_ratio.get(),
               "self-monitoring: exporting traces over OTLP"
           );
       }
   ```
7. Add the test helper, under `#[cfg(test)]` at module level (not inside `mod tests`, so that other modules' tests can call it):
   ```rust
   /// Runs `future` under a subscriber that exports every span (ratio 1.0, logs off, events at
   /// `info` on spans) to an in-process receiver, then flushes. Returns the future's output and
   /// what arrived. The subscriber is this thread's default while `future` runs: a task spawned
   /// on another worker thread is not captured, so tests whose spans start in a spawned task
   /// (rmcp's tool calls) use a current-thread runtime. Works on both runtime flavours.
   #[cfg(test)]
   pub(crate) async fn capture_spans<T>(
       future: impl Future<Output = T>,
   ) -> (T, super::otlp::receiver::Captured)
   ```
   Write the test-only paths in full inside the body, or as `#[cfg(test)] use` lines, so the non-test build has no unused import: `super::otlp::receiver::{Captured, Receiver}`, `axum::http::StatusCode`. Body, in order:
   1. `let receiver = Receiver::start(StatusCode::OK, Duration::ZERO).await;`
   2. Build the TOML:
      ```rust
      let toml = format!(
          "[self_monitoring]\notlp_endpoint = \"{}\"\nlogs = false\ntrace_sample_ratio = 1.0\nlevel = \"info\"\n",
          receiver.base
      );
      ```
      Then `let config = crate::config::load_from(Some(&toml), &[]).expect("capture config");`.
   3. `let (subscriber, observability) = build(&config, LogFormat::Json, BoxMakeWriter::new(std::io::sink), Handle::current()).expect("capture subscriber");`
   4. `crate::test_support::ensure_global_subscriber();`
   5. `let dispatch = tracing::Dispatch::new(subscriber);`
   6. `let value = { let _guard = tracing::dispatcher::set_default(&dispatch); future.await };`
   7. `observability.shutdown().await;`
   8. Return `(value, Captured { requests: receiver.trace_requests() })`.

**Tests** (in `subscriber.rs` `mod tests`).
- Async tests that export use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`.
- Configs come from `crate::config::load_from` with an endpoint at a `Receiver`.
- Spans are local, for example `tracing::info_span!("test.root", a = 1_i64)`. Their target is this test module, which starts with `cetra_`.
- Use `capture_spans` where it fits, and `build` directly where a test needs another config.
- Proto enums: `opentelemetry_proto::tonic::trace::v1::span::SpanKind`, `opentelemetry_proto::tonic::trace::v1::status::StatusCode`.

The tests:
- `stdout_json_is_unchanged_inside_a_span`:
  1. Under `tracing_subscriber::fmt().json().with_writer(buffer_a).finish()`, call `emit()` outside any span. `fn emit() { tracing::info!(project_id = 7, "same line"); }`.
  2. Under `build`'s subscriber, run `info_span!("test.outer", a = 1_i64).in_scope(emit)`. Build it with `log.level = "info"`, an endpoint at a receiver, `logs = false`, `trace_sample_ratio = 1.0`, format `Json` and `buffer_b`.
  3. Parse each output line as `serde_json::Value`, remove `timestamp`, and assert the two are equal.
  4. Also assert that `buffer_b` contains no `"span"` and no `"spans"` key.
- `stdout_pretty_is_unchanged_inside_a_span`: the same with `.pretty()` and `LogFormat::Pretty`. Compare the outputs with the timestamp (the first whitespace-separated token of the first line) removed. Also assert that `buffer_b` does not contain `test.outer`. The pretty output is ANSI-coloured, so a test on line starts would prove nothing.
- `span_target_allowed_keeps_only_cetra_info_spans`:
  - true: `cetra_server::http` at `INFO`; `cetra_storage::spans` at `INFO` and at `WARN`;
  - false: `cetra_server::x` at `DEBUG`; `rmcp::service` at `INFO`; `hyper::proto` at `INFO`; `tower_http::trace` at `INFO`.
- `no_span_layer_without_an_endpoint_when_off_or_at_ratio_zero`: `exporting_traces()` is false for each of:
  - the defaults;
  - an endpoint with `traces = false`;
  - an endpoint with `trace_sample_ratio = 0.0`.

  It is true for an endpoint with the defaults.
- `traces_only_builds_no_log_layer`: an endpoint with `logs = false` gives `exporting_logs()` false and `exporting_traces()` true.
- `spans_carry_their_declared_attributes_only`: in `capture_spans`, `info_span!("test.root", a = 1_i64, b = "x").in_scope(|| {})`. The exported span `test.root`:
  - has attribute keys exactly `{a, b}`: no `thread.*`, `code.*`, `target`, `level`, `busy_ns` or `idle_ns`;
  - has `a` as an int 1;
  - has kind `SPAN_KIND_INTERNAL`;
  - has an empty `parent_span_id`.
- `an_error_event_marks_its_span_as_error`: in `capture_spans`, inside `test.root`, `tracing::error!(error = "boom", "it failed")`. Then:
  - the span's status code is `STATUS_CODE_ERROR` and its message is empty;
  - it has one event named `it failed`, whose attribute keys are exactly `{level, target, error}`, with `error` `boom`;
  - a sibling span with only `tracing::warn!(error = "boom")` (no message) has status `STATUS_CODE_UNSET`. This is the design's setting: `with_error_events_to_status` stays off.
- `span_events_follow_the_export_level`: `build` with `self_monitoring.level = "warn"`, ratio 1.0 and `logs = false`. Inside `test.root`, emit:
  - `info!("info-event")`;
  - `warn!(n = 2_i64, "warn-event")`;
  - `warn!(target: "hyper_util::client", "hyper-event")`;
  - `warn!(target: "cetra_self_monitoring", "failure-event")`.

  After `shutdown`, the span's event names are exactly `{warn-event}`.
- `children_follow_their_root`: in `capture_spans`, `let root = info_span!("test.root"); root.in_scope(|| info_span!("test.child").in_scope(|| {}));`. The child has the root's `trace_id`, and its `parent_span_id` is the root's `span_id`.
- `an_explicit_parent_is_kept`: in `capture_spans`, `let root = info_span!("test.root"); let child = info_span!(parent: &root, "test.child"); drop(child); drop(root);`. The child's `parent_span_id` is the root's `span_id`.
- `other_crates_spans_are_not_exported`: in `capture_spans`:
  ```rust
  info_span!(target: "rmcp::service", "rmcp.span").in_scope(|| info_span!("test.child").in_scope(|| {}))
  ```
  No span is named `rmcp.span`. `test.child` is exported with an empty `parent_span_id`.
- `a_ratio_between_zero_and_one_keeps_whole_traces`: `build` with `trace_sample_ratio = 0.5`, `logs = false` and a receiver. Create 200 roots `test.root`, each with one child `test.child`. After `shutdown`:
  - the number of exported roots is strictly between 0 and 200 (the chance of 0 or 200 is 2⁻¹⁹⁹);
  - the set of the children's `trace_id`s equals the set of the roots' `trace_id`s.
- `shutdown_flushes_both_exports_within_the_flush_timeout`: with a `Receiver` that answers after 30 s, logs and traces both on (ratio 1.0), emit one `warn!` inside one span. `observability.shutdown().await` returns in under 3 s.
- M1's tests stay as they are and must still hold. In particular:
  - `stdout_json_matches_the_former_builder` and `stdout_pretty_matches_the_former_builder`;
  - `the_export_layer_applies_its_own_filter`: its endpoint now also builds a span layer (traces default on), and the receiver accepts `/v1/traces`;
  - `shutdown_returns_within_the_flush_timeout_when_the_endpoint_hangs`.

**Done when.** The file is changed as above and committed.

**Commit:** `Export cetra's spans from the subscriber`

---

### TL2.6 — The storage span catalog and the flush span

**Goal.** Storage defines its three spans in one file, and each flush that commits data runs in a `storage.flush` span.

**Edit:**
- `crates/storage/src/spans.rs` (new)
- `crates/storage/src/lib.rs` (one `mod` line)
- `crates/storage/src/flush.rs`

**Steps.**
1. In `lib.rs`, add `pub(crate) mod spans;` right after `mod snapshot;` (alphabetical order: `snapshot`, `spans`, `sql`).
2. Create `crates/storage/src/spans.rs` with exactly this content:
   ```rust
   //! The storage engine's spans (track TL M2, D-TL-4): the names and attribute keys are
   //! constants, built here by one constructor each. Values are table names, ids, counts and
   //! fixed words: never a path, an object key or an error's text. `tracing` is a facade: these
   //! spans cost little when no layer records them, and the server decides whether they are
   //! exported.

   use tracing::Span;
   use tracing::field::Empty;

   pub(crate) const TABLE: &str = "table";
   pub(crate) const PROJECT_ID: &str = "project_id";
   pub(crate) const KIND: &str = "kind";
   pub(crate) const PARTITIONS: &str = "partitions";
   pub(crate) const FILES: &str = "files";
   pub(crate) const BYTES: &str = "bytes";
   /// `ok` or why not.
   pub(crate) const OUTCOME: &str = "outcome";

   /// `value` as an `i64` attribute, `i64::MAX` when it does not fit (OTel layers record a `u64`
   /// as a string).
   pub(crate) fn count<T: TryInto<i64>>(value: T) -> i64 {
       value.try_into().unwrap_or(i64::MAX)
   }

   /// One flush of `table` that froze rows (`flush::flush_once`).
   pub(crate) fn storage_flush(table: &'static str) -> Span {
       tracing::info_span!(
           "storage.flush",
           { TABLE } = table,
           { PARTITIONS } = Empty,
           { FILES } = Empty,
           { BYTES } = Empty,
           { OUTCOME } = Empty
       )
   }

   /// One compaction job (`compaction::job::Compactor::pass`). `kind` is the job kind label.
   pub(crate) fn storage_compaction(table: &'static str, project: &str, kind: &'static str) -> Span {
       tracing::info_span!(
           "storage.compaction",
           { TABLE } = table,
           { PROJECT_ID } = project,
           { KIND } = kind,
           { FILES } = Empty,
           { BYTES } = Empty,
           { OUTCOME } = Empty
       )
   }

   /// One periodic garbage collection tick (`gc::spawn`).
   pub(crate) fn storage_gc() -> Span {
       tracing::info_span!(
           "storage.gc",
           { FILES } = Empty,
           { BYTES } = Empty,
           { OUTCOME } = Empty
       )
   }
   ```
   Then add, at the end of the file, a test-only capturing layer that `flush.rs`'s test uses:
   ```rust
   /// A layer that records each span's name and fields when it closes (tests only).
   #[cfg(test)]
   pub(crate) mod capture {
       use std::collections::BTreeMap;
       use std::fmt;
       use std::sync::{Arc, Mutex};

       use tracing::field::{Field, Visit};
       use tracing::span::{Attributes, Id, Record};
       use tracing_subscriber::layer::{Context, Layer};
       use tracing_subscriber::registry::LookupSpan;

       /// A closed span: its name and its recorded fields, as text.
       #[derive(Debug, Clone, PartialEq, Eq)]
       pub(crate) struct Closed {
           pub(crate) name: &'static str,
           pub(crate) fields: BTreeMap<&'static str, String>,
       }

       /// Records every span it sees, with its fields, when the span closes.
       #[derive(Debug, Clone, Default)]
       pub(crate) struct Capture {
           closed: Arc<Mutex<Vec<Closed>>>,
       }

       impl Capture {
           /// The spans closed so far, in order.
           pub(crate) fn closed(&self) -> Vec<Closed> {
               self.closed.lock().expect("capture lock").clone()
           }
       }

       struct Fields(BTreeMap<&'static str, String>);

       impl Visit for Fields {
           fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
               self.0.insert(field.name(), format!("{value:?}"));
           }

           fn record_str(&mut self, field: &Field, value: &str) {
               self.0.insert(field.name(), value.to_owned());
           }
       }

       impl<S> Layer<S> for Capture
       where
           S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
       {
           fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
               let mut fields = Fields(BTreeMap::new());
               attrs.record(&mut fields);
               if let Some(span) = ctx.span(id) {
                   span.extensions_mut().insert(fields);
               }
           }

           fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
               if let Some(span) = ctx.span(id) {
                   let mut extensions = span.extensions_mut();
                   if let Some(fields) = extensions.get_mut::<Fields>() {
                       values.record(fields);
                   }
               }
           }

           fn on_close(&self, id: Id, ctx: Context<'_, S>) {
               if let Some(span) = ctx.span(&id) {
                   let fields = span
                       .extensions_mut()
                       .remove::<Fields>()
                       .map(|fields| fields.0)
                       .unwrap_or_default();
                   self.closed.lock().expect("capture lock").push(Closed {
                       name: span.name(),
                       fields,
                   });
               }
           }
       }
   }
   ```
3. In `flush.rs`:
   1. Add `use tracing::Instrument as _;`.
   2. Split `flush_once` at the freeze. Keep everything up to and including `let Some(view) = view else { return Ok(None); };`. Replace the rest of its body with:
      ```rust
          let span = crate::spans::storage_flush(params.table.name());
          let result = flush_frozen(params, view, fresh, &span)
              .instrument(span.clone())
              .await;
          span.record(
              crate::spans::OUTCOME,
              if result.is_ok() { "ok" } else { "error" },
          );
          result.map(Some)
      ```
   3. Add the new function right after `flush_once`. Its body is the code removed from `flush_once` in step 3.2, unchanged except for the three edits below.
      ```rust
      /// [`flush_once`] after the freeze: prepare, stage, write and commit `view` (`fresh`: this
      /// call froze it). Runs in the `storage.flush` span and records its counts there; returns
      /// the committed sequence.
      async fn flush_frozen(
          params: &FlushParams,
          view: FrozenView,
          fresh: bool,
          span: &tracing::Span,
      ) -> Result<u64, FlushError>
      ```
      The three edits:
      - Right after `let partitions = view.partitions.len();`, add `span.record(crate::spans::PARTITIONS, crate::spans::count(partitions));`.
      - Right after the `let Prepared { files, sources, anchor_plan } = prepared;` destructuring, add:
        ```rust
            span.record(crate::spans::FILES, crate::spans::count(files.len()));
            span.record(
                crate::spans::BYTES,
                crate::spans::count(files.iter().map(|file| file.bytes.len()).sum::<usize>()),
            );
        ```
      - The final `Ok(Some(max_seq))` becomes `Ok(max_seq)`. Every `return Err(…)` stays as it is.

      `FrozenView` is already imported (`use crate::memtable::{FrozenView, Memtable, PartitionKey};`).
   4. `run` is unchanged: it still matches `Ok(None)`, `Ok(Some(seq))` and `Err(_)` on `flush_once`.

**Tests:**
- In `spans.rs`, add a `#[cfg(test)] mod tests` after the `capture` module, with the `use` lines `use super::{count, storage_compaction, storage_flush, storage_gc};` and `use tracing::Span;`, and the helper:
  ```rust
  fn shape(span: &Span) -> (&'static str, Vec<&'static str>) {
      let meta = span.metadata().expect("a span keeps its metadata");
      (meta.name(), meta.fields().iter().map(|field| field.name()).collect())
  }
  ```
- In that module, `the_storage_span_catalog_is_fixed`: the shapes of `storage_flush("spans")`, `storage_compaction("spans", "p", "l0-merge")` and `storage_gc()` are exactly:
  ```
  ("storage.flush", ["table", "partitions", "files", "bytes", "outcome"])
  ("storage.compaction", ["table", "project_id", "kind", "files", "bytes", "outcome"])
  ("storage.gc", ["files", "bytes", "outcome"])
  ```
  Each span's target is `cetra_storage::spans`.
- In that module, `count_saturates`: `count(u64::MAX)` is `i64::MAX` and `count(5_usize)` is 5.
- In `flush.rs` `mod tests`, `a_flush_runs_in_a_storage_flush_span`. Use `#[tokio::test(flavor = "current_thread")]`: the flush task must run on this thread, where the capture is the default.
  1. `let capture = crate::spans::capture::Capture::default();`
  2. `let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(capture.clone()));` (`tracing_subscriber::layer::SubscriberExt`).
  3. `let _guard = tracing::dispatcher::set_default(&dispatch);`
  4. Open `Storage::open(anchor_config(dir.path()))`.
  5. Call `append_spans(project(), spans_named(&[at(day(), 12, 0, 0)], "one"))`.
  6. Call `storage.flush_now(Duration::from_secs(10)).await.unwrap()`.
  7. Call `storage.shutdown().await.unwrap()`.
  8. Assert that some closed span is named `storage.flush` with these fields: `table` `spans`, `outcome` `ok`, `files` parsing as an integer ≥ 1, and `bytes` > 0.
  9. Assert that no closed span is named `storage.flush` with `outcome` `error`.

  `tracing-subscriber` is already a dev-dependency of storage (`crates/storage/Cargo.toml`). If `Storage::open` cannot run on a current-thread runtime (a `block_in_place` outside `hooks.rs`), stop and report.

**Done when.** The three files are created or changed as above and committed.

**Commit:** `Run storage flushes in a span`

---

### TL2.7 — Compaction and GC spans

**Goal.** Each compaction job runs in a `storage.compaction` span, and each periodic GC tick in a `storage.gc` span.

**Edit:**
- `crates/storage/src/compaction/job.rs`
- `crates/storage/src/gc.rs`

**Steps.**
1. `compaction/job.rs`:
   1. Add `use tracing::Instrument as _;`.
   2. In `Compactor::pass`, inside `set.spawn(async move { … })`:
      - Replace `let outcome = compactor.run(job, policy).await;` with:
        ```rust
                        let span = crate::spans::storage_compaction(table, &project, kind);
                        let outcome = compactor.run(job, policy).instrument(span.clone()).await;
                        if let Ok(report) = &outcome {
                            span.record(crate::spans::FILES, crate::spans::count(report.input_files));
                            span.record(crate::spans::BYTES, crate::spans::count(report.input_bytes));
                        }
                        span.record(crate::spans::OUTCOME, compaction_outcome(&outcome));
        ```
      - `table`, `project` and `kind` are the locals already built there (`job.table.as_str()`, `job.project.to_string()`, `kind_label(job.kind)`). The log lines that follow stay unchanged.
   3. Add, after `kind_label`:
      ```rust
      /// The `outcome` of a compaction job's span: `ok`, `conflict` (a later pass redoes it),
      /// `deferred` (not enough free disk) or `error`.
      fn compaction_outcome(outcome: &Result<CompactionReport, CompactionError>) -> &'static str {
          match outcome {
              Ok(_) => "ok",
              Err(CompactionError::Commit(CommitError::Conflict { .. })) => "conflict",
              Err(CompactionError::DiskFull) => "deferred",
              Err(_) => "error",
          }
      }
      ```
2. `gc.rs`:
   1. Add `use tracing::Instrument as _;`.
   2. In `spawn`, replace the body of the `_ = interval.tick() => { … }` arm with:
      ```rust
                      _ = interval.tick() => {
                          let span = crate::spans::storage_gc();
                          let failed = gc_tick(&start, &mut next_media, &span)
                              .instrument(span.clone())
                              .await;
                          span.record(crate::spans::OUTCOME, if failed { "error" } else { "ok" });
                      }
      ```
   3. Add, after `spawn`, the function below. Its body is the former arm body, with these changes:
      - The `if let Err(error) = collect_once(…).await { … }` block sets `failed = true` inside its error branch.
      - The `collect_obsolete` call becomes a `match`:
        - `Ok(report)` records `span.record(crate::spans::FILES, crate::spans::count(report.deleted))` and `span.record(crate::spans::BYTES, crate::spans::count(report.bytes))`;
        - `Err(error)` keeps the two former log lines and sets `failed = true`.
      - The media `if let Err(error)` block sets `failed = true` in its error branch.
      - `start.media.finish_drops().await;` stays last.
      - The function starts with `let mut failed = false;` and ends by returning `failed`.

      Every log line stays byte-identical. In the moved body, `start` is the `&GcStart` parameter (the `start.committer` and other field accesses are written the same), and `next_media` is the `&mut` parameter: write `*next_media` where the former body read or assigned `next_media`.
      ```rust
      /// One periodic tick: staged objects, obsolete files, then (daily) media, then half-finished
      /// drops. Records the obsolete files' count and bytes on `span`; returns whether a step failed.
      async fn gc_tick(
          start: &GcStart,
          next_media: &mut tokio::time::Instant,
          span: &tracing::Span,
      ) -> bool
      ```

**Tests** (in `compaction/job.rs` `mod tests`):
- `compaction_outcome_names_each_case`:
  - `compaction_outcome(&Ok(CompactionReport::default()))` is `ok`;
  - `compaction_outcome(&Err(CompactionError::DiskFull))` is `deferred`.

  If `CompactionError::DiskFull` is not a unit variant, stop and report.
- The GC tick has no new test. The span's shape is fixed by `the_storage_span_catalog_is_fixed`, and the tick body is moved unchanged. Its existing tests (`gc.rs`, `the_background_task_compacts` and the soak) must still hold.

**Done when.** The two files are changed as above and committed.

**Commit:** `Run compaction jobs and GC ticks in spans`

---

### TL2.8 — HTTP request and OTLP export spans

**Goal.** Every HTTP request except the health probes runs in an `http.request` span that covers its handler, and every OTLP trace export (HTTP and gRPC) in an `otlp.export` span.

**Edit:**
- `crates/server/src/http/mod.rs`
- `crates/server/src/routes/otlp.rs`
- `crates/server/src/ingest/grpc.rs`

`http/mod.rs` is shared with M6, which may have edited it first, for example with a route or a middleware. M2 replaces only `log_request`'s body and adds tests and imports. Keep M6's lines. If M6 changed `log_request` itself, apply M2's span lines on top of its version and report.

**Names relied on:**
- `crate::observability::spans` (TL2.4);
- `crate::observability::subscriber::capture_spans` and `crate::observability::otlp::receiver::{Captured, string_attribute, int_attribute}` (TL2.3, TL2.5).

**Steps.**
1. `http/mod.rs`:
   1. Add `use tracing::Instrument as _;` and `use crate::observability::spans;`.
   2. Replace `log_request` with:
      ```rust
      /// One `info` event per request, and the `http.request` span around the handler and that
      /// event, so events inside nest under it (track TL M2). The route is the matched template (D150); the URL,
      /// its query and the body are never recorded. The health probes get no span: orchestrators
      /// poll them every few seconds. A 5xx marks the span failed.
      async fn log_request(request: Request, next: Next) -> Response {
          let method = request.method().clone();
          let route = request
              .extensions()
              .get::<MatchedPath>()
              .map_or("<unmatched>", MatchedPath::as_str)
              .to_owned();
          let span = if matches!(route.as_str(), "/healthz" | "/readyz") {
              tracing::Span::none()
          } else {
              spans::http_request(&method, &route)
          };
          let started = Instant::now();
          let response = next.run(request).instrument(span.clone()).await;
          let status = response.status().as_u16();
          span.record(spans::STATUS_CODE, i64::from(status));
          if response.status().is_server_error() {
              span.record(spans::OTEL_STATUS, "error");
          }
          let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
          // Inside the span, so `self_monitoring.level = "info"` records it on `http.request`.
          // The stdout layer sees no span, so the stdout line is byte-identical.
          span.in_scope(|| {
              tracing::info!(
                  target: "http",
                  %method,
                  %route,
                  status,
                  duration_ms,
                  "request"
              );
          });
          response
      }
      ```
      The `request` event's fields and message are unchanged. It is now emitted inside the span: stdout prints the same line, because the stdout layer sees events only (TL2.5), and the span records it as an event when the export level lets `info` through. `span.in_scope` is synchronous; no `.await` happens while the span is entered.
2. `routes/otlp.rs`:
   1. Add `use tracing::Instrument as _;` and `use crate::observability::spans;`.
   2. In `export`:
      1. Right after `let started = Instant::now();`, add `let span = spans::otlp_export(spans::PROTOCOL_HTTP);` and `span.record(spans::PROJECT_ID, tracing::field::display(access.project.id));`.
      2. In the cap branch, add `span.record(spans::OUTCOME, tracing::field::debug(IngestOutcome::quota));` before `return Err(cap.into());`.
      3. Replace the `match export_body(…).await { … }` with:
         ```rust
             match export_body(&state, &access.project, &headers, body, started)
                 .instrument(span.clone())
                 .await
             {
                 Ok(response) => {
                     span.record(spans::OUTCOME, tracing::field::debug(IngestOutcome::ok));
                     Ok(response)
                 }
                 Err(error) => {
                     let outcome = ingest_outcome(&error);
                     state.telemetry.refuse_ingest(Protocol::otlp_http, outcome);
                     span.record(spans::OUTCOME, tracing::field::debug(outcome));
                     log_refusal(access.project.id, &error, started);
                     Err(error.into())
                 }
             }
         ```
   3. In `export_body`, right before the `tracing::info!(target: "ingest", … "request accepted")`, add:
      ```rust
          let span = tracing::Span::current();
          span.record(spans::SPANS, spans::count(receipt.accepted));
          span.record(spans::REJECTED, spans::count(rejections.total()));
          span.record(spans::BYTES, spans::count(request_bytes));
      ```
      (`export_body` runs inside the `otlp.export` span, so it is the current span.)
3. `ingest/grpc.rs`:
   1. Add `use tracing::Instrument as _;` and `use crate::observability::spans;`.
   2. In `GrpcTraces::export`:
      - After `let started = Instant::now();`, add `let span = spans::otlp_export(spans::PROTOCOL_GRPC);`.
      - Change the match to `match export_request(&self.state, request, started).instrument(span.clone()).await {`.
      - In the `Ok(response)` arm, record `span.record(spans::OUTCOME, tracing::field::debug(IngestOutcome::ok));` before returning.
      - The `Err` arm is unchanged.
   3. In `export_request`:
      - Right after `let (project, mut reservation) = take_admitted(&request)?;`, add `tracing::Span::current().record(spans::PROJECT_ID, tracing::field::display(project.id));`.
      - Right before its `tracing::info!(target: "ingest", transport = "grpc", … "request accepted")`, add:
        ```rust
            let span = tracing::Span::current();
            span.record(spans::SPANS, spans::count(receipt.accepted));
            span.record(spans::REJECTED, spans::count(rejected));
            span.record(spans::BYTES, spans::count(bytes));
        ```
   4. Replace `refused`'s body with the code below. A refusal inside `export_request` is recorded on its span; an internal error (`take_admitted`) records no outcome.
      ```rust
          let outcome = ingest_outcome(&error);
          state.telemetry.refuse_ingest(Protocol::otlp_grpc, outcome);
          tracing::Span::current().record(spans::OUTCOME, tracing::field::debug(outcome));
          tonic::Status::from(error)
      ```

**Tests:**
- Each test that captures uses `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` and `capture_spans`.
- The proto kinds and statuses are `span::SpanKind::Server as i32` and `status::StatusCode::Error as i32`.
- An `http.request` span is found with `captured.named("http.request")`.

`http/mod.rs` `mod tests`:
- `a_request_runs_in_an_http_request_span`:
  1. `TestApp::with_routes(Router::new().route("/spans/{id}", get(inside)))`, where `async fn inside() -> &'static str { tracing::info!(n = 1_i64, "inside"); "ok" }`.
  2. `GET /spans/42` inside `capture_spans`.
  3. One `http.request` span with:
     - `http.route` `/spans/{id}`;
     - `http.request.method` `GET`;
     - int `http.response.status_code` 200;
     - kind server;
     - status unset;
     - among its events, one named `inside` and one named `request` (the request line, now inside the span; `capture_spans` exports `info` events). Assert that these are present, not that they are the only ones: auth or CSRF code may add other events.
  4. Nothing in `captured.text()` contains `/spans/42`.
- `a_server_error_marks_the_request_span_as_error`:
  - Route `/boom`: its handler runs `tracing::error!(error = "boom", "handler failed")` and returns `StatusCode::INTERNAL_SERVER_ERROR`. Its span's status is error, and it has an event `handler failed`.
  - Route `/quiet-500`: it returns 500 with no event. Its span's status is also error, through `otel.status_code`.
- `a_client_error_leaves_the_request_span_unset`: `GET /missing` (the fallback) gives one `http.request` span with `http.route` `<unmatched>`, status code 404 and status unset.
- `request_spans_never_carry_the_url_query_or_body`:
  1. Route `/spans/{id}` with `post(take_body)`, where `async fn take_body(_body: axum::body::Bytes) -> StatusCode { StatusCode::NO_CONTENT }`.
  2. Request: `POST /spans/sk-cetra-planted00000000000000000000000000?token=sk-cetra-planted11111111111111111111111111&email=user@example.com`.
  3. The body is `{"prompt":"secret prompt text","key":"sk-cetra-planted22222222222222222222222222"}`, with `.origin(None)` as `json_rejections_hide_the_input` does.
  4. `captured.text()` contains none of `sk-cetra-planted`, `user@example.com` and `secret prompt text`.
  5. The `http.request` span exists, with route `/spans/{id}`.
- `health_probes_have_no_span`: `GET /healthz` and `GET /readyz` inside `capture_spans` give no `http.request` span.
- `an_incoming_traceparent_is_ignored`:
  1. `GET /spans/1` with the header `traceparent: 00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01`.
  2. The span's `trace_id` is not the bytes of `0af7651916cd43dd8448eb211c80319c`.
  3. Its `parent_span_id` is empty.

`routes/otlp.rs` `mod tests`:
- `an_export_runs_in_an_otlp_export_span_under_its_request`:
  1. `TestApp::new()`, `seed()`, and `post_traces(&app, &seeded.ingest_key, "application/x-protobuf", protobuf(&plain_export(2)))` inside `capture_spans`.
  2. One `otlp.export` span with:
     - `protocol` `http`;
     - `project_id` the seeded project's id as a string;
     - int `spans` 2;
     - int `rejected` 0;
     - int `bytes` > 0;
     - `outcome` `ok`;
     - its `parent_span_id` the `span_id` of the one `http.request` span (route `/v1/traces`).
- `a_refused_export_records_its_outcome`: the same with content type `text/plain`. That answers 415, and the `otlp.export` span has `outcome` `invalid`.

`ingest/grpc.rs` `mod tests`:
- `a_grpc_export_runs_in_a_root_otlp_export_span`:
  1. `send(&app.state, &[bearer(&seeded.ingest_key)], &traces())` inside `capture_spans`.
  2. One `otlp.export` span with:
     - `protocol` `grpc`;
     - kind server;
     - an empty `parent_span_id`;
     - int `spans` equal to `i64::try_from(span_count(&traces())).unwrap()` (`span_count` returns `usize`, `int_attribute` returns `i64`);
     - `outcome` `ok`.

**Done when.** The three files are changed as above and committed.

**Commit:** `Trace HTTP requests and OTLP exports`

---

### TL2.9 — Query spans: SQL console and MCP

**Goal.** Every SQL console query, and every MCP tool call, runs in a `query` span under its request. The span never holds SQL text.

**Edit:**
- `crates/server/src/routes/sql.rs`
- `crates/server/src/mcp/mod.rs`

Both files are shared with M6, which may have added `analytics::record(…)` calls in `run_sql` or `call_tool`, or fields to `McpServer`. M2's edits add span lines around the existing calls. Keep M6's calls and fields where they are, inside or outside the instrumented futures, and add `request_span` beside its fields.

**Steps.**
1. `routes/sql.rs`: add `use tracing::Instrument as _;` and `use crate::observability::spans;`. In `run_sql`:
   1. The JSON branch (`let Some(export) = format.export() else { … };`) becomes:
      ```rust
          let Some(export) = format.export() else {
              let span = spans::query(&tracing::Span::current(), spans::QUERY_SQL);
              span.record(spans::PROJECT_ID, tracing::field::display(access.project.id));
              let demo = visitor.map(|_| &state.config.demo);
              let result = run_json(&state, &access, body, demo)
                  .instrument(span.clone())
                  .await;
              match &result {
                  Ok(result) => {
                      span.record(spans::ROWS, spans::count(result.rows.len()));
                      span.record(spans::OUTCOME, "ok");
                  }
                  Err(error) => {
                      span.record(spans::OUTCOME, failure_kind(error));
                  }
              }
              log_end(access.project.id, "json", query_len, started, &result);
              return result.map(|result| error::Json(result).into_response());
          };
      ```
   2. The export path becomes:
      ```rust
          let span = spans::query(&tracing::Span::current(), spans::QUERY_SQL_EXPORT);
          span.record(spans::PROJECT_ID, tracing::field::display(access.project.id));
          match start_export(&state, &access, body, format, export, started)
              .instrument(span.clone())
              .await
          {
              Ok(response) => {
                  span.record(spans::OUTCOME, "ok");
                  Ok(response)
              }
              Err(error) => {
                  span.record(spans::OUTCOME, failure_kind(&error));
                  log_failure(
                      access.project.id,
                      format.label(),
                      query_len,
                      started,
                      &error,
                  );
                  Err(error)
              }
          }
      ```
      If `run_sql`'s existing statements differ from the ones these replace (`demo`, `log_end`, `log_failure` and their arguments), keep the existing ones and only add the span lines. Report the difference.
2. `mcp/mod.rs`: add `use tracing::Instrument as _;` and `use crate::observability::spans;`.
   1. Add the field `request_span: tracing::Span` to `McpServer`, with the doc `/// The request's `http.request` span: rmcp runs a tool call in a spawned task, so the call's span names it as its parent explicitly.`
   2. In `mcp`, change `let server = McpServer { state, user };` to `let server = McpServer { state, user, request_span: tracing::Span::current() };`.
   3. In `call_tool`:
      1. Right after `let started = Instant::now();`, add `let span = spans::query(&self.request_span, spans::QUERY_MCP);`.
      2. In the `tokio::select!`:
         - The tool branch becomes `result = self.run_tool(&request.name, arguments).instrument(span.clone()) => match result {`.
         - In that branch's `Err(error)` arm, record `span.record(spans::OUTCOME, "unknown_tool");` before `return Err(error)`.
         - In the cancelled branch, record `span.record(spans::OUTCOME, "cancelled");` first.
      3. After the `select!`, add `span.record(spans::TOOL, tool);` (`tool` is the `&'static str` from `run_tool`, never `request.name`).
      4. In the `Ok(body)` arm, record `span.record(spans::OUTCOME, "ok");`. In the `Err(error)` arm, record `span.record(spans::OUTCOME, "error");`.

      The log lines are unchanged.

**Tests:**
- `routes/sql.rs` `mod tests`. Both use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` and `crate::observability::subscriber::capture_spans`.
  - `a_sql_query_runs_in_a_query_span_without_its_text`:
    1. `loaded()`, then `post_sql(&app, &seeded, &json!({"query": "SELECT 'sk-cetra-planted00000000000000000000000000' AS planted"}))` inside the capture.
    2. Assert 200, and one `query` span with:
       - `kind` `sql`;
       - `project_id` the seeded project id;
       - int `rows` 1;
       - `outcome` `ok`;
       - `parent_span_id` the `http.request` span's `span_id`.
    3. `captured.text()` contains neither `SELECT` nor `sk-cetra-planted`.
  - `a_failed_sql_query_records_its_failure_kind`: `{"query": "SELEC 1"}` gives 400, and the `query` span's `outcome` is `invalid` (`RunError::Invalid::kind()`).
- `mcp/mod.rs` `mod tests`, using `test_helpers::{app_with_token, call}`:
  - `a_tool_call_runs_in_a_query_span_under_its_request` uses `#[tokio::test(flavor = "current_thread")]`: rmcp runs the call in a spawned task, which only a current-thread runtime keeps on the capturing thread.
    1. `call(&app, &token, "list_projects", &json!({}))` inside `capture_spans`.
    2. One `query` span with:
       - `kind` `mcp`;
       - `tool` `list_projects`;
       - `outcome` `ok`;
       - `parent_span_id` the `span_id` of the `http.request` span (route `/mcp`).
    3. No span name starts with `rmcp`.
  - `an_unknown_tool_records_no_tool_name` uses the same flavour. Call `tools/call` with the name `sk-cetra-planted-tool` through `rpc`. The `query` span has `outcome` `unknown_tool` and no `tool` attribute. `captured.text()` does not contain `sk-cetra-planted-tool`.

**Done when.** The two files are changed as above and committed.

**Commit:** `Trace SQL console queries and MCP tool calls`

---

### TL2.10 — Alert, deletion and retention spans

**Goal.** Each alert rule evaluation, alert delivery, deletion step and retention pass runs in its span, with its outcome.

**Edit:**
- `crates/server/src/alerts/evaluate.rs`
- `crates/server/src/alerts/deliver.rs`
- `crates/server/src/deletion/mod.rs`
- `crates/server/src/retention.rs`

**Steps.** In each file, add `use tracing::Instrument as _;` and `use crate::observability::spans;`. Before renaming a function, grep the file: if the new name already exists, stop and report.
1. `alerts/evaluate.rs`:
   1. Rename `async fn evaluate_rule` to `async fn evaluate_rule_once`. Keep its doc, with `Evaluates one rule.` becoming `Evaluates one rule, inside its span (see [`evaluate_rule`]).`.
   2. Add before it:
      ```rust
      /// [`evaluate_rule_once`] in an `alerts.evaluate` span with its outcome.
      async fn evaluate_rule(
          state: &AppState,
          org: OrgId,
          rule: RuleRow,
          now: i64,
      ) -> (Uuid, bool, Result<Recorded, MetaError>) {
          let span = spans::alert_evaluate(rule.id, rule.project_id);
          let result = evaluate_rule_once(state, org, rule, now)
              .instrument(span.clone())
              .await;
          span.record(spans::OUTCOME, evaluation_outcome(result.1, result.2.as_ref()));
          result
      }

      /// The `outcome` of a rule's span: `write_failed`, `query_failed`, `event`, `stale` or `ok`.
      fn evaluation_outcome(query_failed: bool, written: Result<&Recorded, &MetaError>) -> &'static str {
          match (query_failed, written) {
              (_, Err(_)) => "write_failed",
              (true, Ok(_)) => "query_failed",
              (false, Ok(Recorded::Event { .. })) => "event",
              (false, Ok(Recorded::Stale)) => "stale",
              (false, Ok(Recorded::Written)) => "ok",
          }
      }
      ```
      If `Recorded` has other variants than `Stale`, `Written` and `Event { .. }`, stop and report.
2. `alerts/deliver.rs`:
   1. Rename `async fn deliver_one` to `async fn deliver_one_once`. Its doc keeps `Makes one attempt and records it.` and adds `Runs inside its span (see [`deliver_one`]).`.
   2. Add before it:
      ```rust
      /// [`deliver_one_once`] in an `alerts.deliver` span with its outcome.
      async fn deliver_one(state: &AppState, due: &DueDelivery, now: i64) -> Option<Outcome> {
          let span = spans::alert_deliver(
              due.id,
              due.channel_id,
              due.channel_kind.as_str(),
              due.attempts.saturating_add(1),
          );
          let outcome = deliver_one_once(state, due, now).instrument(span.clone()).await;
          let label = match outcome {
              Some(Outcome::Sent) => "sent",
              Some(Outcome::Retried) => "retry",
              Some(Outcome::Failed) => "failed",
              None => "write_failed",
          };
          span.record(spans::OUTCOME, label);
          outcome
      }
      ```
      If `ChannelKind::as_str` does not return `&'static str`, stop and report.
3. `deletion/mod.rs`, in `run_once`'s loop, after the `if !state.deletions.due(row.id, now) { continue; }` check:
   1. Replace `let result = match row.kind { … };` and the following `match result { … }` with:
      ```rust
              let span = spans::deletion_apply(row.id, row.kind.as_str());
              let result = async {
                  match row.kind {
                      DeletionKind::TraceIds | DeletionKind::Filter | DeletionKind::User => {
                          data_step(state, &row, now).await
                      }
                      DeletionKind::Project => drop_project_step(state, &row, now).await,
                      DeletionKind::Org => drop_org_step(state, &row, now).await,
                  }
              }
              .instrument(span.clone())
              .await;
              span.record(spans::OUTCOME, step_outcome(&result));
              async {
                  match result {
                      Ok(()) => state.deletions.settled(row.id),
                      Err(Step::Retry(label, cause)) => retry(state, &row, label, cause, true).await,
                      Err(Step::RetryForever(label, cause)) => {
                          retry(state, &row, label, cause, false).await
                      }
                      Err(Step::Fail(reason)) => fail(state, &row, reason).await,
                  }
              }
              .instrument(span)
              .await;
      ```
      The arms are the existing ones, moved unchanged. If they differ from these, keep the existing arms and report.
   2. Add after `enum Step`'s `impl`s, or after `from_meta`:
      ```rust
      /// The `outcome` of a deletion step's span: `ok`, `retry` or `failed`.
      fn step_outcome(result: &Result<(), Step>) -> &'static str {
          match result {
              Ok(()) => "ok",
              Err(Step::Retry(..) | Step::RetryForever(..)) => "retry",
              Err(Step::Fail(_)) => "failed",
          }
      }
      ```
4. `retention.rs`:
   1. Rename `pub(crate) async fn retention_once` to `async fn retention_pass` (private). Keep its doc and `# Errors`.
   2. Add before it, with `retention_once`'s former doc and `# Errors` plus the sentence `Runs in a `retention.pass` span.`:
      ```rust
      pub(crate) async fn retention_once(state: &AppState) -> Result<RetentionRun, MetaError> {
          let span = spans::retention_pass();
          let result = retention_pass(state).instrument(span.clone()).await;
          match &result {
              Ok(run) => {
                  span.record(spans::PROJECTS, spans::count(run.projects));
                  span.record(spans::FILES, spans::count(run.files));
                  span.record(spans::FAILURES, spans::count(run.failures));
                  span.record(spans::OUTCOME, "ok");
              }
              Err(_) => {
                  span.record(spans::OUTCOME, "error");
              }
          }
          result
      }
      ```

**Tests:**
- `alerts/evaluate.rs`, in the existing `mod tests` (line ~367; add `evaluation_outcome`, `Recorded`, `MetaError` and `Uuid` to its `use` lines if they are missing), `evaluation_outcome_names_each_case`:
  - `(false, Ok(&Recorded::Written))` gives `ok`;
  - `(false, Ok(&Recorded::Stale))` gives `stale`;
  - `(false, Ok(&Recorded::Event { event_id: Uuid::nil(), deliveries: 1 }))` gives `event`;
  - `(true, Ok(&Recorded::Written))` gives `query_failed`;
  - `(true, Err(&MetaError::NotInitialized))` and `(false, Err(&MetaError::NotInitialized))` give `write_failed`.
- `deletion/mod.rs` `mod tests`, `step_outcome_names_each_case`:
  - `Ok(())` gives `ok`;
  - `Err(Step::Retry("x", None))` and `Err(Step::RetryForever("x", None))` give `retry`;
  - `Err(Step::Fail(FailReason::ResolutionFailed))` gives `failed`.
- `retention.rs` `mod tests`, `a_retention_pass_runs_in_its_span`. Use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`.
  1. `TestApp::new()`, then `retention_once(&app.state)` inside `crate::observability::subscriber::capture_spans`.
  2. One `retention.pass` span, with int `projects`, `files` and `failures` present, and `outcome` `ok`.
- The `alerts.deliver` wiring has no new test: the delivery tests need a channel and a due row. The span's shape is fixed by TL2.4's golden test, and the label mapping is the four-arm `match` written above. The existing alert tests must still hold.

**Done when.** The four files are changed as above and committed.

**Commit:** `Trace alert evaluation and delivery, deletions and retention`

---

### TL2.11 — Eval job, issue candidate and backup spans

**Goal.** Each eval job run, issue candidate run and online backup runs in its span, with its outcome.

**Edit:**
- `crates/server/src/eval/dispatch.rs`
- `crates/server/src/issues/worker.rs`
- `crates/server/src/backup.rs`

**Steps.** In each file, add `use tracing::Instrument as _;` and `use crate::observability::spans;`.
1. `eval/dispatch.rs`, in `run_claimed`:
   1. After `let claims = job.claims;`, add `let span = spans::eval_job(job.row.id, job.row.project_id, job.row.evaluator_id, &job.row.kind);`.
   2. Change `AssertUnwindSafe(execute::run_job(&state, job))` to `AssertUnwindSafe(execute::run_job(&state, job).instrument(span.clone()))`.
   3. In the `match outcome`:
      - the `Ok(Ok(())) => {}` arm becomes `Ok(Ok(())) => { span.record(spans::OUTCOME, "recorded"); }`. `run_job` returns `Ok(())` whenever the job's own result (done, failed, retry, skipped) was written, so `recorded` says that, not that the job succeeded;
      - the `Ok(Err(error))` arm starts with `span.record(spans::OUTCOME, "record_failed");`;
      - the `Err(_panic)` arm starts with `span.record(spans::OUTCOME, "panicked");`.

      The rest is unchanged.
2. `issues/worker.rs`, in `run_candidate`:
   1. After `let source_id = candidate.source_id;`, add `let span = spans::issues_candidate(id, source_id);`.
   2. Change `AssertUnwindSafe(execute_candidate(state, &candidate, claims))` to `AssertUnwindSafe(execute_candidate(state, &candidate, claims).instrument(span.clone()))`.
   3. In the match:
      - `Ok(Ok(())) => {}` becomes `Ok(Ok(())) => { span.record(spans::OUTCOME, "recorded"); }`. As for eval jobs, `execute_candidate` returns `Ok(())` once the candidate's finish is written, whatever that finish is;
      - the `Ok(Err(error))` arm starts with `span.record(spans::OUTCOME, "record_failed");`;
      - the `Err(_panic)` arm starts with `span.record(spans::OUTCOME, "panicked");`.
3. `backup.rs`:
   1. Rename `pub(crate) async fn run` to `async fn take_backup` (private), with its doc unchanged.
   2. Add before it, with the former doc of `run` plus the sentence `Runs in a `backup` span.`:
      ```rust
      pub(crate) async fn run(
          state: &AppState,
          to: &Path,
          meta_only: bool,
      ) -> Result<BackupReport, BackupError> {
          let span = spans::backup();
          let result = take_backup(state, to, meta_only)
              .instrument(span.clone())
              .await;
          match &result {
              Ok(report) => {
                  let mode = match report.mode {
                      BackupMode::Full => "full",
                      BackupMode::MetaOnly => "meta-only",
                  };
                  span.record(spans::MODE, mode);
                  span.record(spans::OBJECTS, spans::count(report.objects));
                  span.record(spans::BYTES, spans::count(report.object_bytes));
                  span.record(spans::OUTCOME, "ok");
              }
              Err(error) => {
                  span.record(spans::OUTCOME, error.code());
              }
          }
          result
      }
      ```
      Dropping `run`'s future drops `take_backup`'s with it, so the cleanup on cancel (`Partial`) is unchanged.

**Tests** (`backup.rs` `mod tests`; `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`; `crate::observability::subscriber::capture_spans`):
- `a_backup_runs_in_its_span`:
  1. `TestApp::new()`, then `run(&app.state, &target, false)`, where `target` is `tempfile::tempdir()`'s path joined with `"b"` (absent, absolute, outside the data directory).
  2. One `backup` span with `mode` `full`, int `objects` ≥ 0, int `bytes` ≥ 0 and `outcome` `ok`.
  3. `captured.text()` does not contain the target path's string.
- `a_refused_backup_records_its_code`: `run(&app.state, Path::new("relative"), false)` fails, and the span's `outcome` is `bad_request`.
- The `eval.job` and `issues.candidate` wiring has no new test: running a job or a candidate needs a judge or a source. The spans' shapes are fixed by TL2.4's golden test, and `job_kind` by its unit test. The existing eval and issue tests must still hold.

**Done when.** The three files are changed as above and committed.

**Commit:** `Trace eval jobs, issue candidates and backups`

---

### TL2.12 — Docs, spec 12 and changelog

**Goal.** The Monitoring page documents the trace export and lists every span. The CHANGELOG, spec 12 and M4's Telemetry page are updated.

**Edit:**
- `website/src/content/docs/docs/self-hosting/monitoring.md`
- `CHANGELOG.md`
- `docs/spec/12-operations.md`
- `website/src/content/docs/docs/self-hosting/telemetry.md` (one table cell)

`CHANGELOG.md`, spec 12 and `telemetry.md` are shared with M3, M5 and M6. M2 appends one CHANGELOG item, adds one spec row and one spec block, and changes one table cell. Keep everything the others added: items, blocks, rows and sections.

**Steps.**
1. `monitoring.md`:
   - Frontmatter `description`: change it to `"Check cetra's health, scrape its Prometheus metrics, export its logs and traces over OTLP and set up alerts."` It is under 160 characters.
   - First paragraph: change "logs" to "logs and traces" where it names the OTLP export.
   - In M1's section `## Export logs over OTLP`, in the `### OpenTelemetry Collector` YAML, add a `traces` pipeline next to `logs`:
     ```yaml
           traces:
             receivers: [otlp]
             exporters: [debug]
     ```
   - Insert this section right **before** `## Alert examples`, after M1's Collector subsection. Keep the heading text: the configuration reference links to `#export-traces-over-otlp`.
     ````markdown
     ## Export traces over OTLP

     With `self_monitoring.otlp_endpoint` set, cetra also sends spans of its own work to
     `<otlp_endpoint>/v1/traces`: HTTP requests, OTLP ingest, SQL console and MCP queries, and
     background work. A traces backend then shows where time goes and which requests fail. The
     request is the same as for logs: protobuf, gzip-compressed, the headers of `otlp_headers`,
     batched every second.

     ```toml
     [self_monitoring]
     otlp_endpoint = "https://otlp.example.com/otlp"
     traces = true              # the default
     trace_sample_ratio = 0.1   # the default: about one trace in ten
     ```

     - **Sampling.** Each root span (an HTTP request, a gRPC export, a background job) is kept
       with the probability `trace_sample_ratio`, and its child spans follow it, so a kept trace
       is whole. `1.0` keeps every trace; `0.0` keeps none.
     - **Errors.** A span is marked failed when an error event is logged inside it, and an HTTP
       request span also when the response is a 5xx. The events inside a span that pass
       `self_monitoring.level` are recorded on it with their fields, as in the log records.
     - **What a span carries.** Ids, counts and fixed values only: never a URL, a query string, a
       request body, SQL text, a name or an email address. The route is the route template, such as
       `/api/v1/projects/{project}/traces`. Spans of the libraries cetra uses are not exported.
     - **Incoming `traceparent` headers are ignored.** A request always starts a new trace: a
       caller cannot attach cetra's spans to its own trace.
     - **The health probes** `/healthz` and `/readyz` get no span.
     - **No `OTEL_*` variable is read**, as for logs.
     - **Another cetra instance** can receive these traces: it stores OTLP traces (not logs).
       Point it at a different instance. An instance that receives its own export creates one
       more trace for each export batch it receives, at the same ratio.

     ### The spans

     | Span | Covers | Attributes |
     |---|---|---|
     | `http.request` | One HTTP request, up to the response head | `http.request.method`, `http.route` (template), `http.response.status_code` |
     | `otlp.export` | One OTLP trace export, over HTTP (inside `http.request`) or gRPC | `protocol`, `project_id`, `spans`, `rejected`, `bytes`, `outcome` |
     | `query` | One SQL console query (`kind` `sql` or `sql_export`) or MCP tool call (`kind` `mcp`) | `kind`, `project_id`, `tool`, `rows`, `outcome` |
     | `eval.job` | One evaluation job | `job_id`, `project_id`, `evaluator_id`, `kind`, `outcome` |
     | `alerts.evaluate` | One alert rule's evaluation | `rule_id`, `project_id`, `outcome` |
     | `alerts.deliver` | One alert delivery attempt | `delivery_id`, `channel_id`, `kind`, `attempt`, `outcome` |
     | `deletion.apply` | One step of one deletion request | `deletion_id`, `kind`, `outcome` |
     | `retention.pass` | The hourly retention pass | `projects`, `files`, `failures`, `outcome` |
     | `issues.candidate` | One issue-detection candidate | `candidate_id`, `source_id`, `outcome` |
     | `backup` | One online backup | `mode`, `objects`, `bytes`, `outcome` |
     | `storage.flush` | One flush of the write-ahead log to Parquet files | `table`, `partitions`, `files`, `bytes`, `outcome` |
     | `storage.compaction` | One compaction job | `table`, `project_id`, `kind`, `files`, `bytes`, `outcome` |
     | `storage.gc` | One garbage collection pass, every 10 minutes | `files`, `bytes`, `outcome` |

     `outcome` is `ok` or a fixed word for what went wrong, such as `invalid`, `timeout` or
     `write_failed`. For `eval.job` and `issues.candidate` it says whether the run's result
     was saved: `recorded` (the job or candidate finished, as done, failed or skipped, and its
     result is in the database), `record_failed` (the result could not be saved; the job is
     retried later) or `panicked`. The evaluation's own result is in the UI, not on the span.
     With `self_monitoring.level = "info"`, each `http.request` span also carries the request
     line as an event.
     ````
     Check by reading that every word used passes `website/scripts/check-terms.mjs`'s banned list, and that no `CETRA_*` name is used other than documented keys. `toml` and `yaml` fences need no sample marker.
2. `CHANGELOG.md` § `[Unreleased]` § `### Added`, appended as the last item, after any item other milestones added. The backslashes below only escape the backticks; they are not part of the text:
   `- \`cetra serve\` can export its own traces (HTTP requests, OTLP ingest, SQL console and MCP queries, background work) to the same OTLP/HTTP endpoint as its logs: \`self_monitoring.traces\` (on once \`self_monitoring.otlp_endpoint\` is set) and \`self_monitoring.trace_sample_ratio\` (default \`0.1\`). Spans carry ids, counts and fixed values only, and incoming \`traceparent\` headers are ignored.`
3. `docs/spec/12-operations.md`:
   1. In the configuration table, right after M1's row `` | `self_monitoring.logs` / `self_monitoring.level` / `self_monitoring.resource` | … ``, add the row below. Match the table's three columns.
      ```markdown
      | `self_monitoring.traces` / `self_monitoring.trace_sample_ratio` | | `true` / `0.1` (0.0–1.0 of root spans, children follow; `/v1/traces` appended) (D-TL-3) |
      ```
   2. In § Self-monitoring, the N M3 blockquote ends with a bare `>` line, then the line `> Server spans over OTLP: track TL M2 (D-TL-4).`. Delete both lines. Then, after M1's `> **Built in TL M1 …` blockquote (and after any TL block another milestone added there), insert a blank line and this new blockquote, so that it does not merge into another quote:
      ```markdown
      > **Built in TL M2 (D-TL-2 to D-TL-4, D-TL-7):**
      > - `self_monitoring.traces` exports cetra's spans to `<otlp_endpoint>/v1/traces` through `tracing-opentelemetry` and cetra's own exporter (same request path as the logs). `BatchSpanProcessor` with the logs' batch values, sampler `ParentBased(TraceIdRatioBased(trace_sample_ratio))`, span limits and resource set explicitly: no `OTEL_*` variable is read. At ratio 0 no span layer is built.
      > - The spans and their attribute keys are constants of two catalogs (`crates/server/src/observability/spans.rs`, `crates/storage/src/spans.rs`), listed on the Monitoring page. Values are ids, counts, fixed words and durations (D-TL-4). Only spans with a `cetra_` target are exported.
      > - Events that pass the log export's filter are recorded on their span; an ERROR event, or a 5xx on `http.request`, sets the span's status to error. No thread, code-location or target attributes.
      > - The stdout layer sees events only, so spans never change stdout lines.
      > - No propagator: an incoming `traceparent` is ignored (D-TL-7). `/healthz` and `/readyz` get no span.
      > - At exit, logs and spans flush in parallel within the same 2 s.
      ```
      If the line `> Server spans over OTLP: track TL M2 (D-TL-4).` is not there, insert the block after M1's blockquote anyway and report.
4. `telemetry.md` (M4's page): in the `features.self_monitoring` row, change `cetra exports its own logs to an OTLP endpoint of yours.` to `cetra exports its own logs or traces to an OTLP endpoint of yours.` If the cell's text differs, change only "logs" to "logs or traces" in it and report. If the row does not exist, change nothing and report.

**Tests.** The coordinator's `just check` runs `check-docs` on these pages. It checks:
- terms and description length;
- env-var names;
- the anchors `#export-traces-over-otlp` (from the configuration reference) and `#export-logs-over-otlp`.

The implementer checks by reading that the new heading's slug is `export-traces-over-otlp`.

**Done when.** The four files are changed as above and committed.

**Commit:** `Document the self-monitoring trace export`

---

## The spans

The full list, settled from the code (D-TL-4). Server spans are built in `crates/server/src/observability/spans.rs` and storage spans in `crates/storage/src/spans.rs`.

Every attribute's type is a string unless the table says int (`i64`). In each Attributes cell, an attribute without a source is set when the span starts, from the constructor's arguments; "(recorded)" marks one set later, with the source given.

| Span | Started in (file, function) | Attributes (type, source) |
|---|---|---|
| `http.request` | `server/src/http/mod.rs`, `log_request` (no span for `/healthz`, `/readyz`) | `otel.kind` = `server`; `http.request.method`: `telemetry::Method::of` as `Debug` (`GET` … `other`); `http.route`: `MatchedPath` or `<unmatched>`; `http.response.status_code`: int (recorded), the response status; `otel.status_code`: `error` (recorded) on 5xx |
| `otlp.export` | `server/src/routes/otlp.rs`, `export`; `server/src/ingest/grpc.rs`, `GrpcTraces::export` | `otel.kind` = `server` (gRPC) or `internal` (HTTP); `protocol` = `http`/`grpc`; `project_id` (recorded): the key's project; `spans`, `rejected`, `bytes`: ints (recorded) from the commit receipt and the request size; `outcome` (recorded): `IngestOutcome` as `Debug` (`ok`, `invalid`, `too_large`, `rate_limited`, `quota`, `unavailable`, `disk_full`) |
| `query` | `server/src/routes/sql.rs`, `run_sql`; `server/src/mcp/mod.rs`, `McpServer::call_tool` (explicit parent: the request span) | `kind` = `sql`/`sql_export`/`mcp`; `project_id` (recorded, SQL); `tool` (recorded, MCP): the static tool name from `run_tool`; `rows`: int (recorded, SQL JSON); `outcome` (recorded): `ok`, `failure_kind` (SQL), `error`/`cancelled`/`unknown_tool` (MCP) |
| `eval.job` | `server/src/eval/dispatch.rs`, `run_claimed` | `job_id`, `project_id`, `evaluator_id`: the job row; `kind`: `job_kind(row.kind)`; `outcome` (recorded): `recorded` (the run's own result, success or failure, was written to `meta.db`)/`record_failed`/`panicked` |
| `alerts.evaluate` | `server/src/alerts/evaluate.rs`, `evaluate_rule` (wrapper) | `rule_id`, `project_id`; `outcome` (recorded): `evaluation_outcome` |
| `alerts.deliver` | `server/src/alerts/deliver.rs`, `deliver_one` (wrapper) | `delivery_id`, `channel_id`, `kind` (`ChannelKind::as_str`), `attempt`: int; `outcome` (recorded): `sent`/`retry`/`failed`/`write_failed` |
| `deletion.apply` | `server/src/deletion/mod.rs`, `run_once` (per due row) | `deletion_id`, `kind` (`DeletionKind::as_str`); `outcome` (recorded): `step_outcome` |
| `retention.pass` | `server/src/retention.rs`, `retention_once` (wrapper) | `projects`, `files`, `failures`: ints (recorded) from `RetentionRun`; `outcome` (recorded): `ok`/`error` |
| `issues.candidate` | `server/src/issues/worker.rs`, `run_candidate` | `candidate_id`, `source_id`; `outcome` (recorded): `recorded` (the run's own result, success or failure, was written to `meta.db`)/`record_failed`/`panicked` |
| `backup` | `server/src/backup.rs`, `run` (wrapper) | `mode` (recorded): `full`/`meta-only`; `objects`, `bytes`: ints (recorded) from `BackupReport`; `outcome` (recorded): `ok` or `BackupError::code` |
| `storage.flush` | `storage/src/flush.rs`, `flush_once` (after a freeze) | `table`: `Table::name`; `partitions`, `files`, `bytes`: ints (recorded); `outcome` (recorded): `ok`/`error` |
| `storage.compaction` | `storage/src/compaction/job.rs`, `Compactor::pass` (per job) | `table`: `FileTable::as_str`; `project_id`; `kind`: `kind_label`; `files`, `bytes`: ints (recorded) from the report's inputs; `outcome` (recorded): `compaction_outcome` |
| `storage.gc` | `storage/src/gc.rs`, `spawn` (per tick) | `files`, `bytes`: ints (recorded) from `ObsoleteReport`; `outcome` (recorded): `ok`/`error` |

## Milestone check (coordinator)

After TL2.12 is merged with whatever M3, M5 and M6 phases ran:
1. `just fmt`.
   - Expect a possible clippy `large_futures` fix-up (workspace lint, 16 KiB). `Instrumented` adds a span to futures that may already be near the limit: `run_json`, `start_export`, `export_body`, `execute_candidate`, `run_job`, `data_step`. The fix is `Box::pin(…)` around the instrumented future, as `serve::run` does.
2. `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check`. Run it with no `HTTP(S)_PROXY`, or with `NO_PROXY` covering 127.0.0.1.
3. `just deny`: licences, and the single versions of the opentelemetry family.
4. By hand:
   - `just run serve` prints the same stdout lines as before M2, in `pretty` and `json`, while requests run.
   - With an OpenTelemetry Collector that has a `traces` pipeline (the `debug` exporter), run with `CETRA_SELF_MONITORING__OTLP_ENDPOINT=http://127.0.0.1:4318` and `CETRA_SELF_MONITORING__TRACE_SAMPLE_RATIO=1`.
     - A few UI page loads show `http.request` spans with route templates.
     - An OTLP export to the server shows `otlp.export` under its request.
     - A SQL console query shows a `query` span without its text.
     - `kill -TERM` flushes them.
5. Update `docs/roadmap.md`'s track TL row (the coordinator's job).
